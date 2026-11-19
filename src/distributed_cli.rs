use clap::{Args, Subcommand};
use horde_rs::{
    Cancellation, TestPlan,
    distributed::{
        AgentLimits, Budget, GruntPolicy, MAX_ENVELOPE_BYTES, ResourceLimits, ThresholdResult,
        controller::{Controller, Grunt},
        transport::{Endpoint, EndpointLimits, Profile},
    },
};
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinSet};
use tokio_rustls::rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
const MAX_SAMPLES: u64 = 10_000;
const MAX_CONCURRENCY: u64 = 8;
const MAX_RATE: u64 = 1_000;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Serve a fixed loopback HTTP GET profile over mutual TLS.
    Grunt(GruntArgs),
    /// Split a request budget across loopback Grunts and print merged JSON results.
    Controller(ControllerArgs),
}

#[derive(Args)]
struct Identity {
    /// Peer trust root (DER certificate).
    #[arg(long)]
    ca: PathBuf,
    /// Local leaf certificate (DER).
    #[arg(long)]
    cert: PathBuf,
    /// Local private key (PKCS#8 DER).
    #[arg(long)]
    key: PathBuf,
}

#[derive(Args)]
pub(crate) struct GruntArgs {
    /// Loopback address to listen on. Port zero selects an available port.
    #[arg(long)]
    listen: SocketAddr,
    /// Locally selected HTTP(S) target with a literal loopback IP.
    #[arg(long)]
    target: reqwest::Url,
    /// Authorized controller leaf certificate (DER). Repeat for additional controllers.
    #[arg(long, required = true)]
    controller_cert: Vec<PathBuf>,
    #[command(flatten)]
    identity: Identity,
}

#[derive(Args)]
pub(crate) struct ControllerArgs {
    /// Grunt loopback address. Repeat for each Grunt in allocation order.
    #[arg(long, required = true)]
    grunt: Vec<SocketAddr>,
    /// Certificate name expected on each Grunt.
    #[arg(long, default_value = "localhost")]
    server_name: String,
    /// Total requests across all Grunts.
    #[arg(short = 'n', long)]
    requests: u64,
    /// Total concurrent requests across all Grunts.
    #[arg(short = 'c', long)]
    concurrency: u64,
    /// Total request starts per second across all Grunts.
    #[arg(long)]
    rate: u64,
    /// Controller deadline in seconds.
    #[arg(long, default_value_t = 30)]
    deadline: u64,
    /// CI threshold for the percentage of failed requests.
    #[arg(long)]
    max_failure_rate: Option<f64>,
    /// CI threshold for the merged p95 in milliseconds.
    #[arg(long)]
    max_p95_ms: Option<f64>,
    #[command(flatten)]
    identity: Identity,
}

impl Identity {
    fn roots(&self) -> Result<RootCertStore, BoxError> {
        let mut roots = RootCertStore::empty();
        roots.add(certificate(&self.ca)?)?;
        Ok(roots)
    }

    fn key(&self) -> Result<PrivateKeyDer<'static>, BoxError> {
        Ok(PrivateKeyDer::Pkcs8(std::fs::read(&self.key)?.into()))
    }
}

fn certificate(path: &Path) -> Result<CertificateDer<'static>, BoxError> {
    Ok(std::fs::read(path)?.into())
}

pub(crate) async fn run(command: Command) -> Result<ExitCode, BoxError> {
    let cancel = Cancellation::new();
    let signals = crate::signals::install(cancel.clone())?;
    match command {
        Command::Grunt(args) => serve(args, signals).await,
        Command::Controller(args) => coordinate(args, cancel, signals).await,
    }
}

async fn serve(
    args: GruntArgs,
    mut signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    if !args.listen.ip().to_canonical().is_loopback() {
        return Err("Grunt listener requires a loopback address".into());
    }
    let target_ip = args
        .target
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .filter(IpAddr::is_loopback)
        .ok_or("Grunt target requires a literal loopback IP")?;
    let mut origin = args.target.clone();
    origin.set_path("/");
    origin.set_query(None);
    let policy = GruntPolicy::new(
        vec![origin],
        vec![target_ip.into()],
        ResourceLimits {
            max_active_runs: 1,
            max_active_histogram_bytes: 8 * 1024 * 1024,
            max_histogram_bytes: MAX_ENVELOPE_BYTES,
            max_request_timeout: Duration::from_secs(10),
            max_redirects: 0,
            max_body_bytes: 0,
            max_header_bytes: 0,
        },
    )?;
    let plan = TestPlan {
        url: args.target,
        requests: 1,
        concurrency: 1,
        timeout: Duration::from_secs(10),
        method: reqwest::Method::GET,
        headers: reqwest::header::HeaderMap::new(),
        body: None,
        duration: None,
        rate: None,
        verbose: false,
        bounded_latency: true,
        reuse_connections: true,
        follow_redirects: false,
        max_redirects: 0,
    };
    let budget = Budget {
        requests: Some(MAX_SAMPLES),
        concurrency: MAX_CONCURRENCY,
        rate_per_second: MAX_RATE,
        duration: None,
    };
    let limits = AgentLimits {
        max_samples: MAX_SAMPLES,
        max_concurrency: MAX_CONCURRENCY,
        max_rate_per_second: MAX_RATE,
        max_duration: Duration::from_secs(60),
    };
    let endpoint = Endpoint::new(
        vec![certificate(&args.identity.cert)?],
        args.identity.key()?,
        args.identity.roots()?,
        args.controller_cert
            .iter()
            .map(|path| certificate(path))
            .collect::<Result<_, _>>()?,
        [(
            "default".into(),
            Profile {
                plan,
                budget,
                limits,
                policy,
            },
        )]
        .into(),
        EndpointLimits {
            max_sessions: 16,
            max_records: 64,
            handshake_timeout: Duration::from_secs(2),
            frame_timeout: Duration::from_secs(3),
            reconnect_grace: Duration::from_secs(2),
        },
    )?;
    let listener = TcpListener::bind(args.listen).await?;
    println!("{}", listener.local_addr()?);
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = signals.wait_for(Option::is_some) => break,
            _ = sessions.join_next(), if !sessions.is_empty() => {}
            accepted = listener.accept(), if sessions.len() < 16 => {
                let (stream, _) = accepted?;
                let endpoint = endpoint.clone();
                sessions.spawn(async move { let _ = endpoint.serve_connection(stream).await; });
            }
        }
    }
    drop(listener);
    sessions.shutdown().await;
    endpoint.shutdown().await;
    Ok(ExitCode::from(
        signals.borrow().map_or(130, |signal| signal.code()),
    ))
}

async fn coordinate(
    args: ControllerArgs,
    cancel: Cancellation,
    signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    let name = ServerName::try_from(args.server_name)?;
    let grunts = args
        .grunt
        .into_iter()
        .map(|address| Grunt {
            address,
            server_name: name.clone(),
        })
        .collect::<Vec<_>>();
    let budget = Budget {
        requests: Some(args.requests),
        concurrency: args.concurrency,
        rate_per_second: args.rate,
        duration: None,
    };
    if args
        .max_failure_rate
        .is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value))
        || args
            .max_p95_ms
            .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err("invalid distributed thresholds".into());
    }
    let controller = Controller::new(
        args.identity.roots()?,
        vec![certificate(&args.identity.cert)?],
        args.identity.key()?,
    )?;
    let report = controller
        .run(
            &grunts,
            "default",
            budget,
            Duration::from_secs(args.deadline),
            cancel,
        )
        .await?;
    let signal = *signals.borrow();
    let aggregate = report.aggregate;
    let partial = aggregate.is_partial() || report.interrupted || signal.is_some();
    let thresholds = aggregate.evaluate_thresholds(args.max_failure_rate, args.max_p95_ms)?;
    let threshold = |configured: Option<f64>, value| match (configured, partial, value) {
        (None, _, _) => "not_configured",
        (_, true, _) | (_, _, ThresholdResult::NotEvaluable) => "not_evaluable",
        (_, _, ThresholdResult::Passed) => "passed",
        (_, _, ThresholdResult::Failed) => "failed",
        _ => "not_configured",
    };
    let agents = aggregate.agents().iter().map(|(agent, status)| {
        use horde_rs::distributed::AgentStatus::*;
        let status = match status {
            Some(Complete) => "complete",
            Some(Cancelled) => "cancelled",
            Some(Failed) => "failed",
            Some(TimedOut) => "timed_out",
            Some(SampleLimit) => "sample_limit",
            Some(AgentDeadline) => "agent_deadline",
            Some(Invalid) => "invalid",
            None => "missing",
        };
        serde_json::json!({"address": agent, "status": status, "elapsed_seconds": aggregate.agent_elapsed(agent).map(|v| v.as_secs_f64())})
    }).collect::<Vec<_>>();
    let percentile = |percent| {
        aggregate
            .histogram()
            .and_then(|histogram| histogram.percentile(percent))
            .map(|v| v.as_secs_f64() * 1000.0)
    };
    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1, "type": "distributed", "partial": partial,
            "interruption": signal.map(|signal| signal.label()),
            "elapsed_seconds": report.elapsed.as_secs_f64(), "agents": agents,
            "metrics": aggregate.metrics(), "requests_per_second": aggregate.throughput(report.elapsed),
            "latency_ms": {"p50": percentile(50), "p95": percentile(95), "p99": percentile(99)},
            "thresholds": {"failure_rate": threshold(args.max_failure_rate, thresholds.failure_rate), "p95": threshold(args.max_p95_ms, thresholds.p95)},
        })
    );
    if let Some(signal) = signal {
        return Ok(ExitCode::from(signal.code()));
    }
    Ok(ExitCode::from(u8::from(
        partial
            || matches!(thresholds.failure_rate, ThresholdResult::Failed)
            || matches!(thresholds.p95, ThresholdResult::Failed),
    )))
}
