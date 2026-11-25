//! Experimental control protocol. Callers supply already accepted TCP streams.
pub use super::control_network::{ListenerPolicy, is_private_control_ip};
use super::scenarios::ScenarioPayload;
pub use super::scenarios::{MAX_SCENARIO_BYTES, ScenarioDescription, ScenarioProfile};
use super::{
    AgentLimits, Budget, Cancellation, CompletedResult, GruntPolicy, LocalOutcome, StopReason,
    encode_histogram, preflight, run_reserved,
};
use crate::TestPlan;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::{Semaphore, watch},
    time::{Instant, sleep_until, timeout},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer},
        server::WebPkiClientVerifier,
    },
};

pub const VERSION: u16 = 1;
const MAX_FRAME: usize = 4096;
pub(super) const MAX_RESULT_METADATA: usize = 32 * 1024;
pub const MAX_RETAINED_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROFILE_NAME: usize = 64;
pub const ALPN: &[u8] = b"horde-grunt/1";

type Id = [u8; 16];
type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u16,
    pub epoch: Id,
    pub run_id: Id,
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Submit {
        profile: String,
    },
    SubmitBudget {
        profile: String,
        budget: Budget,
    },
    DescribeScenario {
        profile: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<Budget>,
    },
    SubmitScenario {
        profile: String,
        revision: [u8; 32],
        budget: Budget,
    },
    Collect,
    Inspect,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Cancelling,
    Complete,
    Cancelled,
    AgentDeadline,
    SampleLimit,
    Failed,
    ResultStorageCapacity,
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Hello { version: u16, epoch: Id },
    State { status: RunStatus },
    Result { histogram_bytes: usize },
    ScenarioProfile { description: ScenarioDescription },
    ScenarioResult { histogram_bytes: Vec<usize> },
    Error { code: ErrorCode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedVersion,
    StaleEpoch,
    InvalidRunId,
    UnknownProfile,
    UnknownRun,
    ConflictingSubmission,
    Capacity,
    Rejected,
    ScenarioRevisionMismatch,
}

/// The protocol selects a locally configured plan and cannot supply target or resource settings.
pub struct Profile {
    pub plan: TestPlan,
    pub budget: Budget,
    pub limits: AgentLimits,
    pub policy: GruntPolicy,
}

#[derive(Default)]
pub struct ProfileSet {
    pub requests: BTreeMap<String, Profile>,
    pub scenarios: BTreeMap<String, ScenarioProfile>,
}

pub struct EndpointLimits {
    pub max_sessions: usize,
    /// Records include terminal runs, retained for the lifetime of the epoch.
    pub max_records: usize,
    /// Retained metadata allowance plus encoded histograms, shared across runs.
    pub max_retained_result_bytes: usize,
    pub handshake_timeout: Duration,
    pub frame_timeout: Duration,
    pub reconnect_grace: Duration,
}

pub struct Endpoint {
    listener_policy: ListenerPolicy,
    tls: TlsAcceptor,
    controllers: Vec<CertificateDer<'static>>,
    profiles: ProfileSet,
    config: EndpointLimits,
    epoch: Id,
    sessions: Semaphore,
    state: Mutex<State>,
}

type RunKey = (usize, Id);

#[derive(Default)]
struct State {
    next_session: u64,
    runs: BTreeMap<RunKey, Run>,
    retained_bytes: usize,
    record_limit_reported: bool,
    result_limit_reported: bool,
}

struct Run {
    profile: String,
    budget: Budget,
    scenario_revision: Option<[u8; 32]>,
    result: Option<StoredResult>,
    status: RunStatus,
    session: Option<u64>,
    cancel: Cancellation,
    disconnected: watch::Sender<Option<Instant>>,
}

#[derive(Clone)]
enum StoredResult {
    Requests {
        metadata: CompletedResult,
        histogram: Arc<[u8]>,
    },
    Scenario(Arc<ScenarioPayload>),
}

impl StoredResult {
    fn bytes(&self) -> usize {
        MAX_RESULT_METADATA
            + match self {
                Self::Requests { histogram, .. } => histogram.len(),
                Self::Scenario(payload) => payload.bytes(),
            }
    }

    async fn write(&self, stream: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        match self {
            Self::Requests {
                metadata,
                histogram,
            } => {
                write_frame(
                    stream,
                    &Reply::Result {
                        histogram_bytes: histogram.len(),
                    },
                )
                .await?;
                write_frame_limited(stream, metadata, MAX_RESULT_METADATA).await?;
                stream.write_all(histogram).await?;
            }
            Self::Scenario(payload) => {
                let histogram_bytes = payload.histograms.iter().map(Vec::len).collect();
                write_frame(stream, &Reply::ScenarioResult { histogram_bytes }).await?;
                write_frame_limited(stream, &payload.metadata, MAX_RESULT_METADATA).await?;
                for histogram in &payload.histograms {
                    stream.write_all(histogram).await?;
                }
            }
        }
        stream.flush().await
    }
}

impl Run {
    fn cancel(&mut self) {
        if self.status == RunStatus::Running {
            self.status = RunStatus::Cancelling;
            self.session = None;
            self.cancel.cancel();
        }
    }

    fn expire(&mut self) {
        let deadline = *self.disconnected.borrow();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.cancel();
        }
    }

    fn attach(&mut self, session: u64) {
        self.expire();
        if self.status == RunStatus::Running {
            self.session = Some(session);
            self.disconnected.send_replace(None);
        }
    }
}

struct Session<'a> {
    endpoint: &'a Endpoint,
    id: u64,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let mut state = self.endpoint.state.lock().unwrap();
        let deadline = Instant::now() + self.endpoint.config.reconnect_grace;
        for run in state
            .runs
            .values_mut()
            .filter(|run| run.session == Some(self.id))
        {
            run.session = None;
            run.disconnected.send_replace(Some(deadline));
        }
    }
}

impl Endpoint {
    /// Authorize specific client leaf certificates in addition to validating their chains.
    pub fn new(
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivateKeyDer<'static>,
        controller_roots: RootCertStore,
        controllers: Vec<CertificateDer<'static>>,
        profiles: BTreeMap<String, Profile>,
        config: EndpointLimits,
    ) -> Result<Arc<Self>, BoxError> {
        Self::with_listener_policy(
            server_chain,
            server_key,
            controller_roots,
            controllers,
            profiles,
            config,
            ListenerPolicy::default(),
        )
    }

    /// Configure the allowed ingress interface and peer addresses in addition to mutual TLS.
    pub fn with_listener_policy(
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivateKeyDer<'static>,
        controller_roots: RootCertStore,
        controllers: Vec<CertificateDer<'static>>,
        profiles: BTreeMap<String, Profile>,
        config: EndpointLimits,
        listener_policy: ListenerPolicy,
    ) -> Result<Arc<Self>, BoxError> {
        Self::with_profiles(
            server_chain,
            server_key,
            controller_roots,
            controllers,
            ProfileSet {
                requests: profiles,
                ..ProfileSet::default()
            },
            config,
            listener_policy,
        )
    }

    /// Configure locally installed GET and scenario profiles.
    pub fn with_profiles(
        server_chain: Vec<CertificateDer<'static>>,
        server_key: PrivateKeyDer<'static>,
        controller_roots: RootCertStore,
        controllers: Vec<CertificateDer<'static>>,
        profiles: ProfileSet,
        config: EndpointLimits,
        listener_policy: ListenerPolicy,
    ) -> Result<Arc<Self>, BoxError> {
        if controllers.is_empty()
            || (profiles.requests.is_empty() && profiles.scenarios.is_empty())
            || profiles
                .scenarios
                .keys()
                .any(|name| !valid_profile_name(name))
            || profiles.requests.iter().any(|(name, profile)| {
                !valid_profile_name(name)
                    || !profile
                        .plan
                        .url
                        .host_str()
                        .and_then(|host| {
                            host.trim_matches(['[', ']'])
                                .parse::<std::net::IpAddr>()
                                .ok()
                        })
                        .is_some_and(|ip| ip.to_canonical().is_loopback())
                    || profile.plan.follow_redirects
            })
            || !(1..=Semaphore::MAX_PERMITS).contains(&config.max_sessions)
            || config.max_records == 0
            || !(1..=MAX_RETAINED_RESULT_BYTES).contains(&config.max_retained_result_bytes)
            || [
                config.handshake_timeout,
                config.frame_timeout,
                config.reconnect_grace,
            ]
            .iter()
            .any(|duration| duration.is_zero() || *duration > Duration::from_secs(3600))
        {
            return Err("invalid Grunt endpoint configuration".into());
        }
        let crypto = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(controller_roots), crypto.clone())
                .build()?;
        let mut tls = ServerConfig::builder_with_provider(crypto)
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(server_chain, server_key)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        // Do not resume sessions so every connection presents its client certificate.
        tls.session_storage = Arc::new(tokio_rustls::rustls::server::NoServerSessionStorage {});
        tls.send_tls13_tickets = 0;
        let mut epoch = [0; 16];
        getrandom::fill(&mut epoch).map_err(|_| "could not generate Grunt epoch")?;
        Ok(Arc::new(Self {
            listener_policy,
            tls: TlsAcceptor::from(Arc::new(tls)),
            controllers,
            profiles,
            epoch,
            sessions: Semaphore::new(config.max_sessions),
            config,
            state: Mutex::new(State::default()),
        }))
    }

    /// Cancel active workloads and wait for their workers to close. Stop accepting
    /// connections and drop session handlers before calling this.
    pub async fn shutdown(&self) {
        for run in self.state.lock().unwrap().runs.values_mut() {
            run.cancel();
        }
        loop {
            if self
                .state
                .lock()
                .unwrap()
                .runs
                .values()
                .all(|run| run.status != RunStatus::Cancelling)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Handle one accepted stream. The caller owns the TCP listener and accept loop.
    pub async fn serve_connection(self: &Arc<Self>, stream: TcpStream) -> Result<(), BoxError> {
        if !self
            .listener_policy
            .admits(stream.local_addr()?, stream.peer_addr()?)
        {
            return Err("Grunt control connection denied by address policy".into());
        }
        stream.set_nodelay(true)?;
        let _permit = self
            .sessions
            .try_acquire()
            .map_err(|_| "Grunt session capacity reached")?;
        let mut stream = timeout(self.config.handshake_timeout, self.tls.accept(stream)).await??;
        let tls = stream.get_ref().1;
        if tls.alpn_protocol() != Some(ALPN) {
            return Err("invalid Grunt application protocol".into());
        }
        let leaf = tls
            .peer_certificates()
            .and_then(|chain| chain.first())
            .ok_or("missing controller certificate")?;
        let controller = self
            .controllers
            .iter()
            .position(|allowed| allowed == leaf)
            .ok_or("unauthorized controller certificate")?;
        let session = {
            let mut state = self.state.lock().unwrap();
            state.next_session = state
                .next_session
                .checked_add(1)
                .ok_or("Grunt session identifiers exhausted")?;
            Session {
                endpoint: self,
                id: state.next_session,
            }
        };
        timeout(
            self.config.frame_timeout,
            write_frame(
                &mut stream,
                &Reply::Hello {
                    version: VERSION,
                    epoch: self.epoch,
                },
            ),
        )
        .await??;
        loop {
            let Some(request): Option<Request> =
                timeout(self.config.frame_timeout, read_frame(&mut stream)).await??
            else {
                return Ok(());
            };
            if matches!(request.operation, Operation::Collect)
                && request.version == VERSION
                && request.epoch == self.epoch
            {
                let result = self
                    .state
                    .lock()
                    .unwrap()
                    .runs
                    .get(&(controller, request.run_id))
                    .and_then(|run| run.result.clone());
                if let Some(result) = result {
                    timeout(self.config.frame_timeout, result.write(&mut stream)).await??;
                    continue;
                }
            }
            let reply = self.handle(controller, session.id, request);
            timeout(self.config.frame_timeout, write_frame(&mut stream, &reply)).await??;
        }
    }

    fn handle(self: &Arc<Self>, controller: usize, session: u64, request: Request) -> Reply {
        let error = |code| Reply::Error { code };
        if request.version != VERSION {
            return error(ErrorCode::UnsupportedVersion);
        }
        if request.epoch != self.epoch {
            return error(ErrorCode::StaleEpoch);
        }
        if request.run_id == [0; 16] {
            return error(ErrorCode::InvalidRunId);
        }
        if let Operation::DescribeScenario { profile, budget } = &request.operation {
            return match self.profiles.scenarios.get(profile) {
                Some(job) if budget.is_none_or(|budget| job.validate_budget(budget).is_ok()) => {
                    Reply::ScenarioProfile {
                        description: job.description.clone(),
                    }
                }
                Some(_) => error(ErrorCode::Rejected),
                None => error(ErrorCode::UnknownProfile),
            };
        }
        let key = (controller, request.run_id);
        let mut state = self.state.lock().unwrap();
        let submit = match &request.operation {
            Operation::Submit { profile } => Some((profile, None, None)),
            Operation::SubmitBudget { profile, budget } => Some((profile, Some(*budget), None)),
            Operation::SubmitScenario {
                profile,
                revision,
                budget,
            } => Some((profile, Some(*budget), Some(*revision))),
            _ => None,
        };
        if let Some(run) = state.runs.get_mut(&key) {
            run.expire();
            if let Some((profile, budget, revision)) = submit
                && (profile != &run.profile
                    || revision != run.scenario_revision
                    || budget.unwrap_or_else(|| self.profiles.requests[profile].budget)
                        != run.budget)
            {
                return error(ErrorCode::ConflictingSubmission);
            }
            match request.operation {
                Operation::Cancel => run.cancel(),
                Operation::Submit { .. }
                | Operation::SubmitBudget { .. }
                | Operation::SubmitScenario { .. }
                | Operation::Inspect => run.attach(session),
                Operation::Collect | Operation::DescribeScenario { .. } => {}
            }
            return Reply::State { status: run.status };
        }
        let Some((profile, requested, revision)) = submit else {
            return error(ErrorCode::UnknownRun);
        };
        let scenario = if let Some(revision) = revision {
            let Some(job) = self.profiles.scenarios.get(profile) else {
                return error(ErrorCode::UnknownProfile);
            };
            if revision != job.description.revision {
                return error(ErrorCode::ScenarioRevisionMismatch);
            }
            Some(job)
        } else {
            None
        };
        let request_profile = if scenario.is_none() {
            let Some(job) = self.profiles.requests.get(profile) else {
                return error(ErrorCode::UnknownProfile);
            };
            Some(job)
        } else {
            None
        };
        if state.runs.len() >= self.config.max_records {
            if !state.record_limit_reported {
                state.record_limit_reported = true;
                eprintln!(
                    "Grunt run_record_capacity: {} of {} records used",
                    state.runs.len(),
                    self.config.max_records
                );
            }
            return error(ErrorCode::Capacity);
        }
        let budget = requested.unwrap_or_else(|| request_profile.expect("request profile").budget);
        // Scenario histograms are per worker and per step. Keep one scenario
        // in flight per endpoint and share its slot with single-URL runs.
        if state.runs.values().any(|run| {
            matches!(run.status, RunStatus::Running | RunStatus::Cancelling)
                && (scenario.is_some() || run.scenario_revision.is_some())
        }) {
            return error(ErrorCode::Rejected);
        }
        if scenario.is_some_and(|job| job.validate_budget(budget).is_err()) {
            return error(ErrorCode::Rejected);
        }
        let single_url = if let Some(job) = request_profile {
            if !fits_profile(budget, job.budget)
                || preflight(budget, job.limits, job.plan.timeout).is_err()
            {
                return error(ErrorCode::Rejected);
            }
            let Ok(reservation) = job.policy.reserve(&job.plan, budget, job.limits) else {
                return error(ErrorCode::Rejected);
            };
            Some((
                (job.plan.clone(), job.limits, job.policy.clone()),
                reservation,
            ))
        } else {
            None
        };
        let (single_url, reservation) = single_url.unzip();
        let cancel = Cancellation::new();
        let (disconnected, changes) = watch::channel(None);
        state.runs.insert(
            key,
            Run {
                profile: profile.clone(),
                budget,
                scenario_revision: revision,
                result: None,
                status: RunStatus::Running,
                session: Some(session),
                cancel: cancel.clone(),
                disconnected,
            },
        );
        let scenario = scenario.cloned();
        let endpoint = self.clone();
        tokio::spawn(async move {
            let result = {
                let run = async {
                    if let Some(job) = scenario {
                        return job
                            .run(budget, cancel)
                            .await
                            .map(|payload| StoredResult::Scenario(Arc::new(payload)));
                    }
                    let (plan, limits, policy) = single_url.expect("request profile");
                    run_requests(plan, budget, limits, &policy, cancel).await
                };
                let watchdog = endpoint.watch_disconnect(key, changes);
                tokio::pin!(run, watchdog);
                tokio::select! {
                    outcome = &mut run => outcome,
                    () = &mut watchdog => run.await,
                }
            };
            let (mut status, mut result) = match result {
                Ok(result) => (RunStatus::Complete, Some(result)),
                Err(status) => (status, None),
            };
            let mut state = endpoint.state.lock().unwrap();
            if let Some(stored) = &result {
                // Charge a fixed metadata allowance and the exact encoded byte count.
                let charge = stored.bytes();
                if charge > endpoint.config.max_retained_result_bytes - state.retained_bytes {
                    if !state.result_limit_reported {
                        state.result_limit_reported = true;
                        eprintln!(
                            "Grunt result_storage_capacity: {} bytes used, {} additional bytes needed, limit {}",
                            state.retained_bytes, charge, endpoint.config.max_retained_result_bytes
                        );
                    }
                    result = None;
                    status = RunStatus::ResultStorageCapacity;
                } else {
                    state.retained_bytes += charge;
                }
            }
            let run = state.runs.get_mut(&key).unwrap();
            run.status = status;
            run.result = result;
            run.session = None;
            drop(reservation);
        });
        Reply::State {
            status: RunStatus::Running,
        }
    }

    async fn watch_disconnect(&self, key: RunKey, mut changes: watch::Receiver<Option<Instant>>) {
        loop {
            let deadline = *changes.borrow_and_update();
            if let Some(deadline) = deadline {
                tokio::select! {
                    result = changes.changed() => if result.is_err() { return; },
                    () = sleep_until(deadline) => {
                        let mut state = self.state.lock().unwrap();
                        let run = state.runs.get_mut(&key).unwrap();
                        run.expire();
                        if run.status != RunStatus::Running { return; }
                    }
                }
            } else if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

async fn run_requests(
    plan: TestPlan,
    budget: Budget,
    limits: AgentLimits,
    policy: &GruntPolicy,
    cancel: Cancellation,
) -> Result<StoredResult, RunStatus> {
    match run_reserved(plan, budget, limits, policy, cancel).await {
        Ok(LocalOutcome::Complete {
            report,
            reason,
            started,
        }) => {
            let metadata = CompletedResult::from_local(&report, reason, started)
                .map_err(|_| RunStatus::Failed)?;
            metadata
                .validate(budget, limits.max_samples)
                .map_err(|_| RunStatus::Failed)?;
            let histogram = encode_histogram(
                report.bounded_latencies.as_ref(),
                metadata.metrics.completed,
            )
            .map_err(|_| RunStatus::Failed)?;
            Ok(StoredResult::Requests {
                metadata,
                histogram: histogram.into(),
            })
        }
        Ok(LocalOutcome::Partial {
            reason: StopReason::Cancelled,
        }) => Err(RunStatus::Cancelled),
        Ok(LocalOutcome::Partial {
            reason: StopReason::AgentDeadline,
        }) => Err(RunStatus::AgentDeadline),
        Ok(LocalOutcome::Partial {
            reason: StopReason::SampleLimit,
        }) => Err(RunStatus::SampleLimit),
        _ => Err(RunStatus::Failed),
    }
}

fn fits_profile(budget: Budget, ceiling: Budget) -> bool {
    let fits = |a: Option<u64>, b: Option<u64>| b.is_none_or(|b| a.is_some_and(|a| a <= b));
    fits(budget.requests, ceiling.requests)
        && budget.concurrency <= ceiling.concurrency
        && budget.rate_per_second <= ceiling.rate_per_second
        && ceiling
            .duration
            .is_none_or(|b| budget.duration.is_some_and(|a| a <= b))
}

/// Validate the identifier used to select a locally configured Grunt profile.
pub fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROFILE_NAME
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

async fn read_frame<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> io::Result<Option<T>> {
    read_frame_limited(stream, MAX_FRAME).await
}

pub(super) async fn read_frame_limited<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> io::Result<Option<T>> {
    let mut size = [0; 4];
    if stream.read(&mut size[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut size[1..]).await?;
    let size = u32::from_be_bytes(size) as usize;
    if !(1..=limit).contains(&size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Grunt frame size",
        ));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Grunt frame"))
}

pub(super) async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
) -> io::Result<()> {
    write_frame_limited(stream, message, MAX_FRAME).await
}

async fn write_frame_limited(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
    limit: usize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Grunt frame too large",
        ));
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests;
