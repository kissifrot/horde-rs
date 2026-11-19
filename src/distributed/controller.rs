//! Coordinator for the experimental Grunt control transport.
use super::{
    AgentOutcome, Aggregate, Budget, Cancellation, CompletedResult, DecodeLimits,
    MAX_ENVELOPE_BYTES, split_budget,
    transport::{
        ALPN, ErrorCode, MAX_RESULT_METADATA, Operation, Reply, Request, RunStatus, VERSION,
        read_frame_limited, write_frame,
    },
};
use futures_util::{StreamExt, future::join_all, stream};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    net::TcpStream,
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{
        ClientConfig, RootCertStore,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    },
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const CANCEL_GRACE: Duration = Duration::from_secs(2);
const MAX_RUN_GRUNTS: usize = 32;
const PREFLIGHT_CONCURRENCY: usize = 16;

pub struct Grunt {
    pub address: SocketAddr,
    pub server_name: ServerName<'static>,
}

pub struct ControllerReport {
    pub aggregate: Aggregate,
    pub elapsed: Duration,
    pub interrupted: bool,
    pub capacity_failures: BTreeMap<String, CapacityFailure>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityFailure {
    RunRecordCapacity,
    ResultStorageCapacity,
}

pub struct Controller {
    tls: TlsConnector,
    private_network: bool,
}

impl Controller {
    pub fn new(
        roots: RootCertStore,
        client_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self, BoxError> {
        let mut tls = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(client_chain, key)?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        Ok(Self {
            tls: TlsConnector::from(Arc::new(tls)),
            private_network: false,
        })
    }

    /// Opt into explicitly configured private Grunt addresses in place of loopback.
    pub fn with_private_network(mut self) -> Self {
        self.private_network = true;
        self
    }

    fn validate_grunts(&self, grunts: &[Grunt]) -> Result<(), BoxError> {
        if grunts.is_empty()
            || grunts.iter().any(|g| {
                let ip = g.address.ip().to_canonical();
                g.address.port() == 0
                    || if self.private_network {
                        !super::control_network::is_private_control_ip(ip)
                    } else {
                        !ip.is_loopback()
                    }
            })
            || grunts
                .iter()
                .map(|g| (g.address.ip().to_canonical(), g.address.port()))
                .collect::<BTreeSet<_>>()
                .len()
                != grunts.len()
        {
            return Err("invalid controller configuration or Grunt address".into());
        }
        if self.private_network
            && grunts
                .iter()
                .map(|g| {
                    g.server_name
                        .to_str()
                        .trim_end_matches('.')
                        .to_ascii_lowercase()
                })
                .collect::<BTreeSet<_>>()
                .len()
                != grunts.len()
        {
            return Err("private Grunts require distinct TLS identities".into());
        }
        Ok(())
    }

    /// Check each peer through the authenticated hello, in input order.
    pub async fn preflight(
        &self,
        grunts: &[Grunt],
        cancel: Cancellation,
    ) -> Result<Vec<Result<[u8; 16], BoxError>>, BoxError> {
        self.validate_grunts(grunts)?;
        Ok(stream::iter(grunts)
            .map(|grunt| {
                let mut cancellation = cancel.subscribe();
                async move {
                    tokio::select! {
                        biased;
                        _ = crate::wait_for_cancel(&mut cancellation) => Err("preflight interrupted".into()),
                        connection = timeout(IO_TIMEOUT, self.connect(grunt)) => {
                            let (epoch, _) = connection.map_err(|_| "timed out waiting for Grunt hello")??;
                            Ok(epoch)
                        }
                    }
                }
            })
            .buffered(PREFLIGHT_CONCURRENCY)
            .collect()
            .await)
    }

    /// Submit each share once, with no resubmission following ambiguous I/O.
    pub async fn run(
        &self,
        grunts: &[Grunt],
        profile: &str,
        budget: Budget,
        deadline: Duration,
        cancel: Cancellation,
    ) -> Result<ControllerReport, BoxError> {
        if grunts.len() > MAX_RUN_GRUNTS
            || deadline.is_zero()
            || deadline > Duration::from_secs(3600)
            || profile.is_empty()
            || profile.len() > 64
        {
            return Err("invalid controller configuration or Grunt address".into());
        }
        self.validate_grunts(grunts)?;
        let shares = split_budget(budget, grunts.len())?;
        let mut run_id = [0; 16];
        getrandom::fill(&mut run_id).map_err(|_| "could not generate run identifier")?;
        let agents = grunts
            .iter()
            .take(shares.len())
            .map(|g| g.address.to_string())
            .collect::<Vec<_>>();
        let mut aggregate = Aggregate::new(run_id, &agents)?;
        let start = Instant::now();
        let outcomes = join_all(
            grunts
                .iter()
                .zip(shares.iter().copied())
                .map(|(grunt, share)| {
                    let cancel = cancel.clone();
                    async move {
                        // Each exchange is bounded independently. An overall deadline drops
                        // the connection and lets the Grunt's disconnect grace take effect.
                        timeout_at(
                            start + deadline,
                            self.drive(grunt, run_id, profile, share, cancel),
                        )
                        .await
                        .ok()
                        .and_then(|result| {
                            result
                                .inspect_err(|error| eprintln!("Grunt {}: {error}", grunt.address))
                                .ok()
                        })
                    }
                }),
        )
        .await;
        let mut capacity_failures = BTreeMap::new();
        for ((name, share), outcome) in agents.iter().zip(shares).zip(outcomes) {
            match outcome {
                Some(Outcome::Result(metadata, histogram)) => {
                    // The aggregator records invalid known results without committing counts.
                    let _ = aggregate.add_completed(
                        run_id,
                        name,
                        *metadata,
                        share,
                        &histogram,
                        decode_limits(share),
                    );
                }
                Some(Outcome::State(status)) => {
                    let outcome = match status {
                        RunStatus::Cancelled => AgentOutcome::Cancelled,
                        RunStatus::AgentDeadline => AgentOutcome::AgentDeadline,
                        RunStatus::SampleLimit => AgentOutcome::SampleLimit,
                        RunStatus::Failed => AgentOutcome::Failed,
                        RunStatus::ResultStorageCapacity => {
                            capacity_failures
                                .insert(name.clone(), CapacityFailure::ResultStorageCapacity);
                            AgentOutcome::Failed
                        }
                        _ => continue,
                    };
                    aggregate.add_noncomplete(run_id, name, outcome)?;
                }
                Some(Outcome::Rejected(code)) => {
                    if code == ErrorCode::Capacity {
                        capacity_failures.insert(name.clone(), CapacityFailure::RunRecordCapacity);
                    }
                    aggregate.add_noncomplete(run_id, name, AgentOutcome::Failed)?
                }
                None => {}
            }
        }
        Ok(ControllerReport {
            aggregate,
            elapsed: start.elapsed(),
            interrupted: cancel.is_cancelled(),
            capacity_failures,
        })
    }

    async fn drive(
        &self,
        grunt: &Grunt,
        run_id: [u8; 16],
        profile: &str,
        budget: Budget,
        cancel: Cancellation,
    ) -> Result<Outcome, BoxError> {
        let mut cancellation = cancel.subscribe();
        let (epoch, mut stream) = tokio::select! {
            biased;
            _ = crate::wait_for_cancel(&mut cancellation) => return Err("submission interrupted".into()),
            connection = timeout(IO_TIMEOUT, self.connect(grunt)) => connection??,
        };
        if cancel.is_cancelled() {
            return Err("submission interrupted".into());
        }
        let mut reply = exchange(
            &mut stream,
            epoch,
            run_id,
            Operation::SubmitBudget {
                profile: profile.to_owned(),
                budget,
            },
        )
        .await?;
        let mut cancel_by = None;
        loop {
            match reply {
                Reply::State {
                    status: RunStatus::Complete,
                } => {
                    let result = timeout(IO_TIMEOUT, async {
                        write_frame(
                            &mut stream,
                            &Request {
                                version: VERSION,
                                epoch,
                                run_id,
                                operation: Operation::Collect,
                            },
                        )
                        .await?;
                        let Reply::Result { histogram_bytes } = read_reply(&mut stream).await?
                        else {
                            return Err("missing completed Grunt result".into());
                        };
                        if !(1..=MAX_ENVELOPE_BYTES).contains(&histogram_bytes) {
                            return Err("invalid Grunt result length".into());
                        }
                        let metadata =
                            read_frame_limited::<CompletedResult>(&mut stream, MAX_RESULT_METADATA)
                                .await?
                                .ok_or("missing Grunt result metadata")?;
                        let mut histogram = vec![0; histogram_bytes];
                        stream.read_exact(&mut histogram).await?;
                        Ok::<_, BoxError>(Outcome::Result(Box::new(metadata), histogram))
                    })
                    .await??;
                    return Ok(result);
                }
                Reply::State {
                    status: RunStatus::Running | RunStatus::Cancelling,
                } => {}
                Reply::State { status } => return Ok(Outcome::State(status)),
                Reply::Error { code } => return Ok(Outcome::Rejected(code)),
                _ => return Err("invalid Grunt operation reply".into()),
            }
            if cancel.is_cancelled() && cancel_by.is_none() {
                cancel_by = Some(Instant::now() + CANCEL_GRACE);
                reply = exchange(&mut stream, epoch, run_id, Operation::Cancel).await?;
                continue;
            }

            if let Some(by) = cancel_by {
                reply = timeout_at(by, async {
                    sleep(Duration::from_millis(25)).await;
                    exchange(&mut stream, epoch, run_id, Operation::Inspect).await
                })
                .await??;
            } else {
                // Listen for cancellation between frames. Dropping an in-progress
                // exchange would lose framing on the authenticated session.
                tokio::select! {
                    _ = crate::wait_for_cancel(&mut cancellation) => continue,
                    () = sleep(Duration::from_millis(25)) => {}
                }
                reply = exchange(&mut stream, epoch, run_id, Operation::Inspect).await?;
            }
        }
    }

    async fn connect(&self, grunt: &Grunt) -> Result<([u8; 16], TlsStream<TcpStream>), BoxError> {
        let tcp = TcpStream::connect(grunt.address).await?;
        tcp.set_nodelay(true)?;
        let mut tls = self.tls.connect(grunt.server_name.clone(), tcp).await?;
        if tls.get_ref().1.alpn_protocol() != Some(ALPN) {
            return Err("unexpected Grunt TLS protocol".into());
        }
        let Reply::Hello {
            version: VERSION,
            epoch,
        } = read_reply(&mut tls).await?
        else {
            return Err("invalid Grunt hello".into());
        };
        Ok((epoch, tls))
    }
}

enum Outcome {
    State(RunStatus),
    Result(Box<CompletedResult>, Vec<u8>),
    Rejected(ErrorCode),
}

fn decode_limits(budget: Budget) -> DecodeLimits {
    DecodeLimits {
        max_envelope_bytes: MAX_ENVELOPE_BYTES,
        max_samples: budget.requests.unwrap_or(100_000),
        assigned_requests: budget.requests,
    }
}

async fn read_reply(stream: &mut TlsStream<TcpStream>) -> Result<Reply, BoxError> {
    read_frame_limited(stream, MAX_RESULT_METADATA)
        .await?
        .ok_or_else(|| "Grunt closed the connection".into())
}

async fn exchange(
    stream: &mut TlsStream<TcpStream>,
    epoch: [u8; 16],
    run_id: [u8; 16],
    operation: Operation,
) -> Result<Reply, BoxError> {
    timeout(IO_TIMEOUT, async {
        write_frame(
            stream,
            &Request {
                version: VERSION,
                epoch,
                run_id,
                operation,
            },
        )
        .await?;
        read_reply(stream).await
    })
    .await?
}
