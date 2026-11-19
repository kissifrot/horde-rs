use clap::{Args, Subcommand};
use horde_rs::{
    Cancellation, TestPlan,
    distributed::{
        AgentLimits, Budget, GruntPolicy, MAX_ENVELOPE_BYTES, ResourceLimits, ThresholdResult,
        controller::{Controller, Grunt},
        transport::{Endpoint, EndpointLimits, ListenerPolicy, MAX_RETAINED_RESULT_BYTES, Profile},
    },
};
use serde::Deserialize;
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
const MAX_RUN_RECORDS: usize = 64;

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Serve a fixed loopback HTTP GET profile over mutual TLS.
    Grunt(GruntArgs),
    /// Split a request budget across Grunts and print merged JSON results.
    Controller(ControllerArgs),
    /// Verify Grunt control connections through the authenticated hello.
    Preflight(PreflightArgs),
}

#[derive(Args)]
struct IdentityArgs {
    /// Peer trust root (DER certificate). Repeat for overlapping CA rotation.
    #[arg(long, required_unless_present = "config")]
    ca: Vec<PathBuf>,
    /// Local leaf certificate (DER).
    #[arg(long, required_unless_present = "config")]
    cert: Option<PathBuf>,
    /// Local private key (PKCS#8 DER).
    #[arg(long, required_unless_present = "config")]
    key: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct GruntArgs {
    /// Load local Grunt settings from a JSON file.
    #[arg(long, conflicts_with_all = ["listen", "allow_controller_ip", "target", "controller_cert", "ca", "cert", "key"])]
    config: Option<PathBuf>,
    /// Specific loopback or private address to listen on. Port zero selects an available port.
    #[arg(long, required_unless_present = "config")]
    listen: Option<SocketAddr>,
    /// Exact private IP of an allowed controller. Repeat to authorize multiple IPs.
    #[arg(long)]
    allow_controller_ip: Vec<IpAddr>,
    /// Locally selected HTTP(S) target with a literal loopback IP.
    #[arg(long, required_unless_present = "config")]
    target: Option<reqwest::Url>,
    /// Authorized controller leaf certificate (DER). Repeat for additional controllers.
    #[arg(long, required_unless_present = "config")]
    controller_cert: Vec<PathBuf>,
    #[command(flatten)]
    identity: IdentityArgs,
}

#[derive(Args)]
struct Peers {
    /// Load the inventory path, control policy and TLS settings from a JSON file.
    #[arg(long, conflicts_with_all = ["grunt", "inventory", "private_network", "server_name", "ca", "cert", "key"])]
    config: Option<PathBuf>,
    /// Grunt socket address, optionally followed by =TLS_IDENTITY. Repeat in allocation order.
    #[arg(long, required_unless_present_any = ["inventory", "config"], value_name = "ADDRESS[=TLS_IDENTITY]", value_parser = parse_grunt)]
    grunt: Vec<GruntSpec>,
    /// Read Grunt addresses and TLS identities from a JSON inventory.
    #[arg(long, conflicts_with = "grunt")]
    inventory: Option<PathBuf>,
    /// Connect to explicitly listed private Grunt addresses instead of loopback.
    #[arg(long)]
    private_network: bool,
    /// Default TLS identity for entries that omit one. Defaults to localhost on loopback and the Grunt IP in private mode.
    #[arg(long)]
    server_name: Option<String>,
    #[command(flatten)]
    identity: IdentityArgs,
}

#[derive(Args)]
pub(crate) struct PreflightArgs {
    #[command(flatten)]
    peers: Peers,
}

#[derive(Args)]
pub(crate) struct ControllerArgs {
    #[command(flatten)]
    peers: Peers,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    ca: Vec<PathBuf>,
    cert: PathBuf,
    key: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GruntConfig {
    listen: SocketAddr,
    #[serde(default)]
    allow_controller_ip: Vec<IpAddr>,
    #[serde(deserialize_with = "deserialize_target")]
    target: reqwest::Url,
    controller_cert: Vec<PathBuf>,
    tls: Identity,
    max_records: Option<usize>,
    max_retained_result_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControllerConfig {
    inventory: PathBuf,
    #[serde(default)]
    private_network: bool,
    server_name: Option<String>,
    tls: Identity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    grunts: Vec<InventoryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryEntry {
    address: SocketAddr,
    tls_identity: Option<String>,
}

fn deserialize_target<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<reqwest::Url, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, kind: &str) -> Result<T, BoxError> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open {kind} {}: {error}", path.display()))?;
    serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|error| format!("invalid {kind} {}: {error}", path.display()).into())
}

impl IdentityArgs {
    fn load(self) -> Result<Identity, BoxError> {
        Ok(Identity {
            ca: self.ca,
            cert: self.cert.ok_or("missing --cert")?,
            key: self.key.ok_or("missing --key")?,
        })
    }
}

impl GruntArgs {
    fn load(self) -> Result<GruntConfig, BoxError> {
        if let Some(path) = self.config {
            let mut settings: GruntConfig = read_json(&path, "Grunt configuration")?;
            let directory = path.parent().unwrap_or(Path::new("."));
            settings.tls.resolve_paths(directory);
            for cert in &mut settings.controller_cert {
                *cert = directory.join(&*cert);
            }
            Ok(settings)
        } else {
            Ok(GruntConfig {
                listen: self.listen.ok_or("missing --listen")?,
                allow_controller_ip: self.allow_controller_ip,
                target: self.target.ok_or("missing --target")?,
                controller_cert: self.controller_cert,
                tls: self.identity.load()?,
                max_records: None,
                max_retained_result_bytes: None,
            })
        }
    }
}

impl Peers {
    fn prepare(self) -> Result<(Controller, Vec<Grunt>), BoxError> {
        let (inventory, private_network, server_name, tls) = if let Some(path) = self.config {
            let settings: ControllerConfig = read_json(&path, "controller configuration")?;
            let directory = path.parent().unwrap_or(Path::new("."));
            let mut tls = settings.tls;
            tls.resolve_paths(directory);
            (
                Some(directory.join(settings.inventory)),
                settings.private_network,
                settings.server_name,
                tls,
            )
        } else {
            (
                self.inventory,
                self.private_network,
                self.server_name,
                self.identity.load()?,
            )
        };
        let mut specs = self.grunt;
        if let Some(path) = inventory {
            let inventory: Inventory = read_json(&path, "Grunt inventory")?;
            for entry in inventory.grunts {
                let identity = entry
                    .tls_identity
                    .map(ServerName::try_from)
                    .transpose()
                    .map_err(|error| {
                        format!("invalid TLS identity for {}: {error}", entry.address)
                    })?;
                specs.push(GruntSpec {
                    address: entry.address,
                    identity,
                });
            }
        }
        let default = server_name.map(ServerName::try_from).transpose()?;
        let grunts = specs
            .into_iter()
            .map(|spec| spec.resolve(default.as_ref(), private_network))
            .collect();
        let controller = Controller::new(tls.roots()?, vec![certificate(&tls.cert)?], tls.key()?)?;
        Ok((
            if private_network {
                controller.with_private_network()
            } else {
                controller
            },
            grunts,
        ))
    }
}

#[derive(Clone)]
struct GruntSpec {
    address: SocketAddr,
    identity: Option<ServerName<'static>>,
}

fn parse_grunt(value: &str) -> Result<GruntSpec, String> {
    let (address, identity) = value
        .split_once('=')
        .map_or((value, None), |(address, name)| (address, Some(name)));
    Ok(GruntSpec {
        address: address
            .parse()
            .map_err(|_| "expected a socket address (IP:port)".to_owned())?,
        identity: identity
            .map(|name| ServerName::try_from(name).map(|identity| identity.to_owned()))
            .transpose()
            .map_err(|_| "expected a DNS name or IP as the Grunt TLS identity".to_owned())?,
    })
}

impl GruntSpec {
    fn resolve(self, default: Option<&ServerName<'static>>, private: bool) -> Grunt {
        let server_name = self
            .identity
            .or_else(|| default.cloned())
            .unwrap_or_else(|| {
                if private {
                    ServerName::from(self.address.ip().to_canonical())
                } else {
                    ServerName::try_from("localhost").expect("localhost is a valid DNS name")
                }
            });
        Grunt {
            address: self.address,
            server_name,
        }
    }
}

impl Identity {
    fn resolve_paths(&mut self, directory: &Path) {
        for path in self.ca.iter_mut().chain([&mut self.cert, &mut self.key]) {
            *path = directory.join(&*path);
        }
    }

    fn roots(&self) -> Result<RootCertStore, BoxError> {
        if self.ca.is_empty() {
            return Err("at least one peer CA certificate is required".into());
        }
        let mut roots = RootCertStore::empty();
        for ca in &self.ca {
            roots.add(certificate(ca)?)?;
        }
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
        Command::Grunt(args) => serve(args.load()?, signals).await,
        Command::Controller(args) => coordinate(args, cancel, signals).await,
        Command::Preflight(args) => preflight(args, cancel, signals).await,
    }
}

async fn serve(
    args: GruntConfig,
    mut signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    let max_records = args.max_records.unwrap_or(MAX_RUN_RECORDS);
    if !(1..=MAX_RUN_RECORDS).contains(&max_records) {
        return Err(format!("Grunt max_records must be between 1 and {MAX_RUN_RECORDS}").into());
    }
    let max_retained_result_bytes = args
        .max_retained_result_bytes
        .unwrap_or(MAX_RETAINED_RESULT_BYTES);
    if !(1..=MAX_RETAINED_RESULT_BYTES).contains(&max_retained_result_bytes) {
        return Err(format!(
            "Grunt max_retained_result_bytes must be between 1 and {MAX_RETAINED_RESULT_BYTES}"
        )
        .into());
    }
    let listener_policy = ListenerPolicy::for_listener(args.listen, args.allow_controller_ip)?;
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
    let endpoint = Endpoint::with_listener_policy(
        vec![certificate(&args.tls.cert)?],
        args.tls.key()?,
        args.tls.roots()?,
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
            max_records,
            max_retained_result_bytes,
            handshake_timeout: Duration::from_secs(2),
            frame_timeout: Duration::from_secs(3),
            reconnect_grace: Duration::from_secs(2),
        },
        listener_policy,
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

async fn preflight(
    args: PreflightArgs,
    cancel: Cancellation,
    signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    let (controller, grunts) = args.peers.prepare()?;
    let results = controller.preflight(&grunts, cancel).await?;
    let signal = *signals.borrow();
    let ready = signal.is_none() && results.iter().all(Result::is_ok);
    let peers = grunts
        .iter()
        .zip(results)
        .map(|(grunt, result)| {
            let (status, epoch, error) = match result {
                Ok(epoch) => ("ready", Some(epoch), None),
                Err(error) => ("failed", None, Some(error.to_string())),
            };
            serde_json::json!({
                "address": grunt.address.to_string(),
                "tls_identity": grunt.server_name.to_str(),
                "status": status,
                "epoch": epoch,
                "error": error,
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1, "type": "distributed_preflight", "ready": ready,
            "interruption": signal.map(|signal| signal.label()), "grunts": peers,
        })
    );
    Ok(ExitCode::from(
        signal.map_or(u8::from(!ready), |signal| signal.code()),
    ))
}

async fn coordinate(
    args: ControllerArgs,
    cancel: Cancellation,
    signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
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
    let (controller, grunts) = args.peers.prepare()?;
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
        let mut entry = serde_json::json!({"address": agent, "status": status, "elapsed_seconds": aggregate.agent_elapsed(agent).map(|v| v.as_secs_f64())});
        if let Some(reason) = report.capacity_failures.get(agent) {
            entry["reason"] = serde_json::json!(reason);
        }
        entry
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_grunts_default_to_ip_and_explicit_names_override_the_fallback() {
        for (address, ip) in [("10.0.0.2:7071", "10.0.0.2"), ("[fd12::2]:7071", "fd12::2")] {
            let grunt = parse_grunt(address).unwrap().resolve(None, true);
            assert!(matches!(grunt.server_name, ServerName::IpAddress(_)));
            assert_eq!(grunt.server_name.to_str(), ip);
        }
        let fallback = ServerName::try_from("fallback.test").unwrap();
        let grunt = parse_grunt("10.0.0.2:7071=grunt-a.test")
            .unwrap()
            .resolve(Some(&fallback), true);
        assert_eq!(grunt.server_name.to_str(), "grunt-a.test");
        assert!(parse_grunt("10.0.0.2:7071=").is_err());
    }
}
