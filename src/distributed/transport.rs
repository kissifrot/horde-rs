//! Experimental loopback protocol. Callers supply already accepted TCP streams.
use super::{
    AgentLimits, Budget, Cancellation, GruntPolicy, LocalOutcome, StopReason, preflight,
    run_reserved,
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
    Submit { profile: String },
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
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Hello { version: u16, epoch: Id },
    State { status: RunStatus },
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
}

/// The protocol selects a locally configured plan and cannot supply target or resource settings.
pub struct Profile {
    pub plan: TestPlan,
    pub budget: Budget,
    pub limits: AgentLimits,
    pub policy: GruntPolicy,
}

pub struct EndpointLimits {
    pub max_sessions: usize,
    /// Records include terminal runs, retained for the lifetime of the epoch.
    pub max_records: usize,
    pub handshake_timeout: Duration,
    pub frame_timeout: Duration,
    pub reconnect_grace: Duration,
}

pub struct Endpoint {
    tls: TlsAcceptor,
    controllers: Vec<CertificateDer<'static>>,
    profiles: BTreeMap<String, Profile>,
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
}

struct Run {
    profile: String,
    status: RunStatus,
    session: Option<u64>,
    cancel: Cancellation,
    disconnected: watch::Sender<Option<Instant>>,
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
        if controllers.is_empty()
            || profiles.is_empty()
            || profiles.keys().any(|name| !valid_profile_name(name))
            || !(1..=Semaphore::MAX_PERMITS).contains(&config.max_sessions)
            || config.max_records == 0
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
            tls: TlsAcceptor::from(Arc::new(tls)),
            controllers,
            profiles,
            epoch,
            sessions: Semaphore::new(config.max_sessions),
            config,
            state: Mutex::new(State::default()),
        }))
    }

    /// Handle one accepted loopback stream. The caller owns the TCP listener and accept loop.
    pub async fn serve_connection(self: &Arc<Self>, stream: TcpStream) -> Result<(), BoxError> {
        if !stream.local_addr()?.ip().to_canonical().is_loopback()
            || !stream.peer_addr()?.ip().to_canonical().is_loopback()
        {
            return Err("Grunt transport requires loopback".into());
        }
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
            let Some(request) =
                timeout(self.config.frame_timeout, read_frame(&mut stream)).await??
            else {
                return Ok(());
            };
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
        let key = (controller, request.run_id);
        let mut state = self.state.lock().unwrap();
        if let Some(run) = state.runs.get_mut(&key) {
            run.expire();
            match request.operation {
                Operation::Submit { profile } if profile != run.profile => {
                    return error(ErrorCode::ConflictingSubmission);
                }
                Operation::Cancel => run.cancel(),
                Operation::Submit { .. } | Operation::Inspect => run.attach(session),
            }
            return Reply::State { status: run.status };
        }
        let Operation::Submit { profile } = request.operation else {
            return error(ErrorCode::UnknownRun);
        };
        let Some(job) = self.profiles.get(&profile) else {
            return error(ErrorCode::UnknownProfile);
        };
        if state.runs.len() >= self.config.max_records {
            return error(ErrorCode::Capacity);
        }
        if preflight(job.budget, job.limits, job.plan.timeout).is_err() {
            return error(ErrorCode::Rejected);
        }
        let Ok(reservation) = job.policy.reserve(&job.plan, job.budget, job.limits) else {
            return error(ErrorCode::Rejected);
        };
        let (plan, budget, limits, policy) =
            (job.plan.clone(), job.budget, job.limits, job.policy.clone());
        let cancel = Cancellation::new();
        let (disconnected, changes) = watch::channel(None);
        state.runs.insert(
            key,
            Run {
                profile,
                status: RunStatus::Running,
                session: Some(session),
                cancel: cancel.clone(),
                disconnected,
            },
        );
        let endpoint = self.clone();
        tokio::spawn(async move {
            let outcome = {
                let run = run_reserved(plan, budget, limits, &policy, cancel);
                let watchdog = endpoint.watch_disconnect(key, changes);
                tokio::pin!(run, watchdog);
                tokio::select! {
                    outcome = &mut run => outcome,
                    () = &mut watchdog => run.await,
                }
            };
            let status = match outcome {
                Ok(LocalOutcome::Complete { .. }) => RunStatus::Complete,
                Ok(LocalOutcome::Partial {
                    reason: StopReason::Cancelled,
                }) => RunStatus::Cancelled,
                Ok(LocalOutcome::Partial {
                    reason: StopReason::AgentDeadline,
                }) => RunStatus::AgentDeadline,
                Ok(LocalOutcome::Partial {
                    reason: StopReason::SampleLimit,
                }) => RunStatus::SampleLimit,
                _ => RunStatus::Failed,
            };
            let mut state = endpoint.state.lock().unwrap();
            let run = state.runs.get_mut(&key).unwrap();
            run.status = status;
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

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROFILE_NAME
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

async fn read_frame<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> io::Result<Option<T>> {
    let mut size = [0; 4];
    if stream.read(&mut size[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut size[1..]).await?;
    let size = u32::from_be_bytes(size) as usize;
    if !(1..=MAX_FRAME).contains(&size) {
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

async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    message: &impl Serialize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
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
