mod inventory;
#[cfg(feature = "mcp")]
pub(crate) mod mcp;
mod scenarios;

use crate::text_report::{TextReport, Tone};
use clap::{Args, Subcommand, ValueEnum};
use horde_rs::{
    Cancellation, TestPlan,
    distributed::{
        AgentLimits, AgentStatus, Budget, GruntPolicy, MAX_ENVELOPE_BYTES, ResourceLimits,
        ThresholdResult, ThresholdResults,
        controller::{CapacityFailure, Controller, ControllerReport, Grunt},
        transport::{
            Endpoint, EndpointLimits, ListenerPolicy, MAX_RETAINED_RESULT_BYTES, Profile,
            ProfileSet,
        },
    },
};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
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
const DEFAULT_PROFILE: &str = "default";
const DEFAULT_DEADLINE_SECONDS: u64 = 30;
const DEFAULT_PROFILE_LIMITS: AgentLimits = AgentLimits {
    max_samples: MAX_SAMPLES,
    max_concurrency: MAX_CONCURRENCY,
    max_rate_per_second: MAX_RATE,
    max_duration: Duration::from_secs(60),
};

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Serve a fixed loopback HTTP GET profile over mutual TLS.
    Grunt(GruntArgs),
    /// Split a request budget across Grunts and print merged results.
    Controller(ControllerArgs),
    /// Verify Grunt control connections through the authenticated hello.
    Preflight(PreflightArgs),
    /// Manage the Grunts available for future runs.
    Inventory(inventory::InventoryArgs),
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
    #[arg(long, group = "inventory_source", conflicts_with_all = ["grunt", "inventory", "private_network", "server_name", "ca", "cert", "key"])]
    config: Option<PathBuf>,
    /// Grunt socket address, optionally followed by =TLS_IDENTITY. Repeat in allocation order.
    #[arg(long, required_unless_present_any = ["inventory", "config"], value_name = "ADDRESS[=TLS_IDENTITY]", value_parser = parse_grunt)]
    grunt: Vec<GruntSpec>,
    /// Read Grunt addresses and TLS identities from a JSON inventory.
    #[arg(long, group = "inventory_source", conflicts_with = "grunt")]
    inventory: Option<PathBuf>,
    /// Add a peer to the inventory before connecting. Repeat to add several peers.
    #[arg(long, requires = "inventory_source", value_name = "ADDRESS[=TLS_IDENTITY]", value_parser = parse_grunt)]
    add_grunt: Vec<GruntSpec>,
    /// Remove an address from the inventory before connecting. Repeat to remove several peers.
    #[arg(long, requires = "inventory_source", value_name = "ADDRESS")]
    remove_grunt: Vec<SocketAddr>,
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
    /// Check that every Grunt has the same revision of this installed scenario.
    #[arg(long, value_parser = parse_scenario_name)]
    scenario: Option<String>,
}

#[derive(Args)]
pub(crate) struct ControllerArgs {
    #[command(flatten)]
    peers: Peers,
    /// Total requests across all Grunts.
    #[arg(
        short = 'n',
        long,
        required_unless_present = "scenario",
        conflicts_with = "scenario"
    )]
    requests: Option<u64>,
    /// Name of a scenario installed on the Grunts.
    #[arg(long, requires = "iterations")]
    scenario: Option<String>,
    /// Total scenario iterations across all Grunts.
    #[arg(long, requires = "scenario")]
    iterations: Option<u64>,
    /// Total concurrent workers across all Grunts.
    #[arg(short = 'c', long)]
    concurrency: u64,
    /// Total request or iteration starts per second across all Grunts.
    #[arg(long)]
    rate: u64,
    /// Controller deadline in seconds.
    #[arg(long, default_value_t = DEFAULT_DEADLINE_SECONDS)]
    deadline: u64,
    /// CI threshold for the percentage of failed requests or iterations.
    #[arg(long)]
    max_failure_rate: Option<f64>,
    /// CI threshold for the merged p95 in milliseconds.
    #[arg(long)]
    max_p95_ms: Option<f64>,
    /// Output format for the final report.
    #[arg(long, value_enum, default_value = "json")]
    format: ReportFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    Json,
    Text,
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
    #[serde(default, deserialize_with = "deserialize_target")]
    target: Option<reqwest::Url>,
    #[serde(default)]
    scenarios: BTreeMap<String, PathBuf>,
    user_agent: Option<String>,
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

impl ControllerConfig {
    fn read(path: &Path) -> Result<Self, BoxError> {
        let mut settings: Self = read_json(path, "controller configuration")?;
        settings
            .tls
            .resolve_paths(path.parent().unwrap_or(Path::new(".")));
        Ok(settings)
    }

    fn inventory_path(&self, config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&self.inventory)
    }
}

fn deserialize_target<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<reqwest::Url>, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map(Some)
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
            for scenario in settings.scenarios.values_mut() {
                *scenario = directory.join(&*scenario);
            }
            Ok(settings)
        } else {
            Ok(GruntConfig {
                listen: self.listen.ok_or("missing --listen")?,
                allow_controller_ip: self.allow_controller_ip,
                target: self.target,
                scenarios: BTreeMap::new(),
                user_agent: None,
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
            let settings = ControllerConfig::read(&path)?;
            let inventory = settings.inventory_path(&path);
            (
                Some(inventory),
                settings.private_network,
                settings.server_name,
                settings.tls,
            )
        } else {
            (
                self.inventory,
                self.private_network,
                self.server_name,
                self.identity.load()?,
            )
        };
        let default = server_name.map(ServerName::try_from).transpose()?;
        let controller = build_controller(&tls, private_network)?;
        let resolve = |specs| resolve_grunts(specs, default.as_ref(), private_network);
        let grunts = match inventory {
            Some(path) if !self.add_grunt.is_empty() || !self.remove_grunt.is_empty() => {
                inventory::Inventory::edit(&path, |inventory| {
                    let mut changed = false;
                    for address in self.remove_grunt {
                        inventory.remove(address)?;
                        changed = true;
                    }
                    for grunt in self.add_grunt {
                        changed |= inventory.add(grunt)?;
                    }
                    let grunts = resolve(inventory.specs()?);
                    controller.validate_grunts(&grunts)?;
                    Ok((changed, grunts))
                })?
            }
            Some(path) => resolve(inventory::Inventory::read(&path)?.specs()?),
            None => resolve(self.grunt),
        };
        Ok((controller, grunts))
    }
}

fn build_controller(tls: &Identity, private_network: bool) -> Result<Controller, BoxError> {
    let controller = Controller::new(tls.roots()?, vec![certificate(&tls.cert)?], tls.key()?)?;
    Ok(if private_network {
        controller.with_private_network()
    } else {
        controller
    })
}

fn resolve_grunts(
    specs: Vec<GruntSpec>,
    default: Option<&ServerName<'static>>,
    private_network: bool,
) -> Vec<Grunt> {
    specs
        .into_iter()
        .map(|spec| spec.resolve(default, private_network))
        .collect()
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
    let signals = crate::signals::install(cancel.clone(), false)?;
    match command {
        Command::Grunt(args) => serve(args.load()?, signals).await,
        Command::Controller(args) => coordinate(args, cancel, signals).await,
        Command::Preflight(args) => preflight(args, cancel, signals).await,
        Command::Inventory(args) => args.run(),
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
    let requests = args
        .target
        .map(|target| {
            request_profile(target, args.user_agent.as_deref())
                .map(|profile| (DEFAULT_PROFILE.into(), profile))
        })
        .transpose()?
        .into_iter()
        .collect();
    let scenarios = scenarios::load(args.scenarios, args.user_agent)?;
    let endpoint = Endpoint::with_profiles(
        vec![certificate(&args.tls.cert)?],
        args.tls.key()?,
        args.tls.roots()?,
        args.controller_cert
            .iter()
            .map(|path| certificate(path))
            .collect::<Result<_, _>>()?,
        ProfileSet {
            requests,
            scenarios,
        },
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

fn request_profile(target: reqwest::Url, user_agent: Option<&str>) -> Result<Profile, BoxError> {
    let mut headers = HeaderMap::new();
    let mut max_header_bytes = 0;
    if let Some(user_agent) = user_agent {
        let value = HeaderValue::from_str(user_agent)
            .map_err(|_| "Grunt user_agent is not a valid HTTP header value")?;
        // The controller selects this local profile without supplying headers.
        max_header_bytes = USER_AGENT.as_str().len() + value.as_bytes().len();
        headers.insert(USER_AGENT, value);
    }
    let target_ip = target
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .filter(IpAddr::is_loopback)
        .ok_or("Grunt target requires a literal loopback IP")?;
    let mut origin = target.clone();
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
            max_header_bytes,
        },
    )?;
    let plan = TestPlan {
        url: target,
        requests: 1,
        concurrency: 1,
        timeout: Duration::from_secs(10),
        method: reqwest::Method::GET,
        headers,
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
    Ok(Profile {
        plan,
        budget,
        limits: DEFAULT_PROFILE_LIMITS,
        policy,
    })
}

async fn preflight(
    args: PreflightArgs,
    cancel: Cancellation,
    signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    let (controller, grunts) = args.peers.prepare()?;
    let (ready, report, signal) = if let Some(name) = args.scenario {
        let results = controller
            .preflight_scenario(&grunts, &name, cancel)
            .await?;
        let signal = *signals.borrow();
        let (ready, report) = scenarios::preflight_report(&name, &grunts, results, signal);
        (ready, report, signal)
    } else {
        let results = controller.preflight(&grunts, cancel).await?;
        let signal = *signals.borrow();
        let (ready, report) = preflight_report(&grunts, results, signal);
        (ready, report, signal)
    };
    println!("{report}");
    Ok(ExitCode::from(
        signal.map_or(u8::from(!ready), |signal| signal.code()),
    ))
}

fn parse_scenario_name(name: &str) -> Result<String, &'static str> {
    if horde_rs::distributed::transport::valid_profile_name(name) {
        Ok(name.to_owned())
    } else {
        Err("use 1 to 64 ASCII letters, digits, hyphens or underscores")
    }
}

fn preflight_report(
    grunts: &[Grunt],
    results: Vec<Result<[u8; 16], BoxError>>,
    signal: Option<crate::signals::Interrupt>,
) -> (bool, serde_json::Value) {
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
    let report = serde_json::json!({
        "schema_version": 1, "type": "distributed_preflight", "ready": ready,
        "interruption": signal.map(|signal| signal.label()), "grunts": peers,
    });
    (ready, report)
}

async fn coordinate(
    args: ControllerArgs,
    cancel: Cancellation,
    signals: tokio::sync::watch::Receiver<Option<crate::signals::Interrupt>>,
) -> Result<ExitCode, BoxError> {
    let budget = Budget {
        requests: args.requests.or(args.iterations),
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
    if let Some(name) = args.scenario {
        let report = controller
            .run_scenario(
                &grunts,
                &name,
                budget,
                Duration::from_secs(args.deadline),
                cancel,
            )
            .await;
        if report.is_err() && signals.borrow().is_some() {
            return Ok(crate::signals::report(
                &signals,
                matches!(args.format, ReportFormat::Json),
            ));
        }
        return scenarios::print_report(
            &name,
            report?,
            args.format,
            args.max_failure_rate,
            args.max_p95_ms,
            *signals.borrow(),
        );
    }
    let report = controller
        .run(
            &grunts,
            DEFAULT_PROFILE,
            budget,
            Duration::from_secs(args.deadline),
            cancel,
        )
        .await?;
    let signal = *signals.borrow();
    let aggregate = &report.aggregate;
    let partial = aggregate.is_partial() || report.interrupted || signal.is_some();
    let thresholds = aggregate.evaluate_thresholds(args.max_failure_rate, args.max_p95_ms)?;
    let thresholds_failed = matches!(thresholds.failure_rate, ThresholdResult::Failed)
        || matches!(thresholds.p95, ThresholdResult::Failed);
    let effective = |limit: Option<f64>, value| match (limit, partial) {
        (None, _) => ThresholdResult::NotConfigured,
        (Some(_), true) => ThresholdResult::NotEvaluable,
        (Some(_), false) => value,
    };
    let thresholds = ThresholdResults {
        failure_rate: effective(args.max_failure_rate, thresholds.failure_rate),
        p95: effective(args.max_p95_ms, thresholds.p95),
    };
    let percentile = |percent| {
        aggregate
            .histogram()
            .and_then(|histogram| histogram.percentile(percent))
            .map(|v| v.as_secs_f64() * 1000.0)
    };
    match args.format {
        ReportFormat::Json => {
            let threshold = |value| match value {
                ThresholdResult::NotConfigured => "not_configured",
                ThresholdResult::NotEvaluable => "not_evaluable",
                ThresholdResult::Passed => "passed",
                ThresholdResult::Failed => "failed",
            };
            let agents = aggregate.agents().iter().map(|(agent, status)| {
                let (status, _, _) = agent_status(*status);
                let mut entry = serde_json::json!({"address": agent, "status": status, "elapsed_seconds": aggregate.agent_elapsed(agent).map(|v| v.as_secs_f64())});
                if let Some(reason) = report.capacity_failures.get(agent) {
                    entry["reason"] = serde_json::json!(reason);
                }
                entry
            }).collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1, "type": "distributed", "partial": partial,
                    "interruption": signal.map(|signal| signal.label()),
                    "elapsed_seconds": report.elapsed.as_secs_f64(), "agents": agents,
                    "metrics": aggregate.metrics(), "requests_per_second": aggregate.throughput(report.elapsed),
                    "latency_ms": {"p50": percentile(50), "p95": percentile(95), "p99": percentile(99)},
                    "thresholds": {"failure_rate": threshold(thresholds.failure_rate), "p95": threshold(thresholds.p95)},
                })
            );
        }
        ReportFormat::Text => {
            let (status, tone) = if partial {
                ("PARTIAL", Tone::Warning)
            } else if thresholds_failed {
                ("THRESHOLD FAILED", Tone::Error)
            } else if aggregate.metrics().failed > 0 {
                ("COMPLETE WITH ERRORS", Tone::Warning)
            } else {
                ("COMPLETE", Tone::Good)
            };
            let mut output = TextReport::new("Distributed", status, tone);
            if let Some(signal) = signal {
                output.field("Interruption:", signal.label());
            }
            let metrics = aggregate.metrics();
            output.section("Requests");
            output.field(
                "Completed requests:",
                format_args!(
                    "{} / {}",
                    metrics.completed,
                    args.requests.expect("request count")
                ),
            );
            if partial {
                output.line(format_args!("Counts include confirmed Grunt results only."));
            }
            output.field("Failed requests:", metrics.failed);
            output.field(
                "Elapsed:",
                format_args!("{:.3} s", report.elapsed.as_secs_f64()),
            );
            if let Some(throughput) = aggregate.throughput(report.elapsed) {
                output.field("Requests/sec:", format_args!("{throughput:.2}"));
            }
            output.section("Latency (merged HDR histogram, approximate upper bounds)");
            if aggregate.histogram().is_some() {
                for p in [50, 95, 99] {
                    if let Some(value) = percentile(p) {
                        output.field(&format!("Latency p{p}:"), format_args!("{value:.2} ms"));
                    }
                }
            } else {
                output.line(format_args!("No samples"));
            }
            write_grunts(&mut output, &report);
            if args.max_failure_rate.is_some() || args.max_p95_ms.is_some() {
                output.section("Thresholds");
            }
            for (label, limit, unit, result) in [
                (
                    "Failure rate:",
                    args.max_failure_rate,
                    "%",
                    thresholds.failure_rate,
                ),
                ("Latency p95:", args.max_p95_ms, " ms", thresholds.p95),
            ] {
                let Some(limit) = limit else { continue };
                let verdict = match result {
                    ThresholdResult::Passed => output.status("PASS", Tone::Good),
                    ThresholdResult::Failed => output.status("FAIL", Tone::Error),
                    _ => output.status("NOT EVALUABLE", Tone::Warning),
                };
                output.field(label, format_args!("limit {limit:.2}{unit} — {verdict}"));
            }
            output.print()?;
        }
    }
    Ok(ExitCode::from(signal.map_or(
        u8::from(partial || thresholds_failed),
        |signal| signal.code(),
    )))
}

fn agent_status(status: Option<AgentStatus>) -> (&'static str, &'static str, Tone) {
    match status {
        Some(AgentStatus::Complete) => ("complete", "Complete", Tone::Good),
        Some(AgentStatus::Cancelled) => ("cancelled", "Cancelled", Tone::Warning),
        Some(AgentStatus::Failed) => ("failed", "Failed", Tone::Error),
        Some(AgentStatus::TimedOut) => ("timed_out", "Timed out", Tone::Warning),
        Some(AgentStatus::SampleLimit) => ("sample_limit", "Sample limit reached", Tone::Warning),
        Some(AgentStatus::AgentDeadline) => {
            ("agent_deadline", "Grunt deadline reached", Tone::Warning)
        }
        Some(AgentStatus::Invalid) => ("invalid", "Invalid result", Tone::Error),
        None => ("missing", "Missing", Tone::Warning),
    }
}

fn write_grunts(output: &mut TextReport, report: &ControllerReport) {
    output.section("Grunts");
    for (address, status) in report.aggregate.agents() {
        let (_, label, tone) = agent_status(*status);
        let status = output.status(label, tone);
        let detail = match report.capacity_failures.get(address) {
            Some(CapacityFailure::RunRecordCapacity) => " (run record capacity reached)".to_owned(),
            Some(CapacityFailure::ResultStorageCapacity) => {
                " (result storage capacity reached)".to_owned()
            }
            None => report
                .aggregate
                .agent_elapsed(address)
                .map_or_else(String::new, |elapsed| {
                    format!(" ({:.3} s)", elapsed.as_secs_f64())
                }),
        };
        output.line(format_args!("  {address}  {status}{detail}"));
    }
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
