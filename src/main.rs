use clap::Parser;
use horde_rs::scenario::{Counts, Scenario, ScenarioOptions, StartWindow, run_scenario_config};
use horde_rs::{LatencyStats, TestPlan, run};
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT},
};
use std::{
    error::Error,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

struct ScenarioDiagnostic(String);

impl std::fmt::Display for ScenarioDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Debug for ScenarioDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl Error for ScenarioDiagnostic {}

/// For the Horde! Run an HTTP load test from this machine.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Zug zug! Only test services you are authorized to load test."
)]
struct Args {
    /// HTTP or HTTPS target URL.
    #[arg(required_unless_present = "scenario", conflicts_with = "scenario")]
    url: Option<Url>,
    /// Run a JSON scenario instead of testing a single URL.
    #[arg(long, conflicts_with_all = ["method", "headers", "body_file", "no_connection_reuse", "max_p95_ms"])]
    scenario: Option<PathBuf>,
    /// Validate a scenario without making HTTP requests.
    #[arg(long, requires = "scenario", conflicts_with_all = ["auto_referer", "requests", "concurrency", "timeout", "user_agent", "duration", "rate", "verbose", "bounded_latency", "max_failure_rate", "max_step_p95_ms", "follow_redirects", "max_redirects"])]
    validate: bool,
    /// Derive Referer between scenario steps (requires --scenario).
    #[arg(long, requires = "scenario")]
    auto_referer: bool,
    /// Total number of requests or scenario iterations.
    #[arg(short = 'n', long)]
    requests: Option<usize>,
    /// Maximum concurrent requests or scenario users.
    #[arg(short = 'c', long, default_value_t = 1)]
    concurrency: usize,
    /// Per-request timeout in seconds, including the response body.
    #[arg(short = 't', long, default_value_t = 30)]
    timeout: u64,
    /// HTTP method.
    #[arg(short = 'X', long, default_value = "GET")]
    method: Method,
    /// HTTP header, repeatable (e.g. -H 'Accept: application/json').
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// User-Agent header (overrides any User-Agent supplied with -H).
    #[arg(short = 'A', long)]
    user_agent: Option<String>,
    /// Read the request body from a file and reuse it for each request.
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Stop starting requests or scenario iterations after this many seconds.
    #[arg(long)]
    duration: Option<NonZeroU64>,
    /// Target starts per second for requests or scenario iterations (1 to 1000000000).
    #[arg(long)]
    rate: Option<u32>,
    /// Log request progress to stderr (can affect performance).
    #[arg(short = 'v', long)]
    verbose: bool,
    /// Use a bounded histogram for approximate latency percentiles.
    #[arg(long)]
    bounded_latency: bool,
    /// Disable HTTP connection reuse.
    #[arg(long)]
    no_connection_reuse: bool,
    /// Maximum failed requests as a percentage (0 to 100).
    #[arg(long, allow_hyphen_values = true)]
    max_failure_rate: Option<f64>,
    /// Maximum p95 request latency in milliseconds.
    #[arg(long, allow_hyphen_values = true)]
    max_p95_ms: Option<f64>,
    /// Maximum p95 latency in milliseconds for each scenario step (requires --scenario).
    #[arg(long, requires = "scenario", allow_hyphen_values = true)]
    max_step_p95_ms: Option<f64>,
    /// Print a machine-readable JSON summary instead of the text report.
    #[arg(long)]
    json: bool,
    /// Follow HTTP redirects.
    #[arg(short = 'L', long)]
    follow_redirects: bool,
    /// Maximum redirects per request (requires -L; default: 10).
    #[arg(long, requires = "follow_redirects")]
    max_redirects: Option<NonZeroUsize>,
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn Error + Send + Sync>> {
    let args = Args::parse();
    if args
        .max_failure_rate
        .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
    {
        return Err("max failure rate must be a finite percentage between 0 and 100".into());
    }
    if args.max_p95_ms.is_some_and(|v| !v.is_finite() || v < 0.0) {
        return Err("max p95 must be a finite, non-negative number of milliseconds".into());
    }
    if args
        .max_step_p95_ms
        .is_some_and(|v| !v.is_finite() || v < 0.0)
    {
        return Err("max step p95 must be a finite, non-negative number of milliseconds".into());
    }
    if let Some(path) = &args.scenario {
        let input = std::fs::read(path).map_err(|error| {
            ScenarioDiagnostic(format!("cannot read scenario {}: {error}", path.display()))
        })?;
        let scenario = Scenario::from_json(&input)
            .map_err(|error| ScenarioDiagnostic(format!("scenario {}: {error}", path.display())))?;
        if args.validate {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"schema_version": 1, "type": "scenario_validation", "valid": true})
                );
            } else {
                println!("Scenario is valid.");
            }
            return Ok(ExitCode::SUCCESS);
        }
        let mut report = run_scenario_config(
            scenario,
            args.requests.unwrap_or(if args.duration.is_some() {
                usize::MAX
            } else {
                1
            }),
            args.concurrency,
            Duration::from_secs(args.timeout),
            ScenarioOptions {
                verbose: args.verbose,
                max_redirects: args
                    .follow_redirects
                    .then(|| args.max_redirects.map_or(10, NonZeroUsize::get)),
                auto_referer: args.auto_referer,
                bounded_latency: args.bounded_latency,
                user_agent: args.user_agent.clone(),
                duration: args
                    .duration
                    .map(|seconds| Duration::from_secs(seconds.get())),
                rate: args.rate,
            },
        )
        .await?;
        fn mean(count: &Counts) -> Option<f64> {
            (count.completed > 0)
                .then(|| count.elapsed.as_secs_f64() * 1000.0 / count.completed as f64)
        }
        fn percentile(stats: &Option<LatencyStats>, p: u32) -> Option<f64> {
            stats
                .as_ref()
                .and_then(|stats| stats.percentile(p))
                .map(|duration| duration.as_secs_f64() * 1000.0)
        }
        let scenario_stats = report.scenarios.take_latency_stats();
        let step_stats: Vec<_> = report
            .steps
            .iter_mut()
            .map(|(_, count)| count.take_latency_stats())
            .collect();
        let failure_rate = (report.scenarios.completed > 0)
            .then(|| report.scenarios.failed as f64 * 100.0 / report.scenarios.completed as f64);
        let completed_per_second = if report.elapsed.is_zero() {
            0.0
        } else {
            report.scenarios.completed as f64 / report.elapsed.as_secs_f64()
        };
        let start_window_method = match report.starts.method {
            StartWindow::Duration => "duration",
            StartWindow::IterationLimit => "iteration_limit",
        };
        let failure_passed = args
            .max_failure_rate
            .map(|limit| failure_rate.is_some_and(|value| value <= limit));
        let step_checks: Vec<_> = report
            .steps
            .iter()
            .zip(&step_stats)
            .map(|((name, _), stats)| {
                let p95 = percentile(stats, 95);
                (
                    name,
                    p95,
                    args.max_step_p95_ms
                        .map(|limit| p95.is_some_and(|value| value <= limit)),
                )
            })
            .collect();
        let step_passed = args.max_step_p95_ms.map(|_| {
            step_checks
                .iter()
                .all(|(_, _, passed)| *passed == Some(true))
        });
        let exit_code = if failure_passed == Some(false) || step_passed == Some(false) {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
        if args.json {
            let steps: Vec<_> = report.steps.iter().zip(&step_stats).map(|((name, count), stats)| serde_json::json!({"name": name, "completed": count.completed, "failed": count.failed, "mean_latency_ms": mean(count), "latency_ms": {"p50": percentile(stats, 50), "p90": percentile(stats, 90), "p95": percentile(stats, 95), "p99": percentile(stats, 99), "p100": percentile(stats, 100)}, "status_codes": count.status_codes, "failure_details": count.failure_details})).collect();
            let checks: Vec<_> = step_checks.iter().map(|(name, observed, passed)| serde_json::json!({"name": name, "observed": observed, "passed": passed})).collect();
            let output = serde_json::json!({"schema_version": 1, "type": "scenario", "name": report.name, "elapsed_seconds": report.elapsed.as_secs_f64(), "latency_method": if args.bounded_latency { "hdr_histogram_upper_bound_3_significant_digits" } else { "exact" }, "scenarios": {"completed": report.scenarios.completed, "failed": report.scenarios.failed, "target_starts_per_second": args.rate, "started_per_second": report.starts.per_second, "start_measurement": {"started": report.starts.started, "window_seconds": report.starts.window.as_secs_f64(), "method": start_window_method}, "completed_per_second": completed_per_second, "mean_duration_ms": mean(&report.scenarios), "duration_ms": {"p50": percentile(&scenario_stats, 50), "p95": percentile(&scenario_stats, 95), "p99": percentile(&scenario_stats, 99)}}, "steps": steps, "thresholds": {"failure_rate_percent": args.max_failure_rate.map(|limit| serde_json::json!({"limit":limit,"observed":failure_rate,"passed":failure_passed})), "step_p95_ms": args.max_step_p95_ms.map(|limit| serde_json::json!({"limit":limit,"passed":step_passed,"steps":checks}))}});
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!(
                "For the Horde!\nScenario: {}\nCompleted scenarios: {}\nFailed scenarios:    {}\nElapsed:             {:.3} s",
                report.name,
                report.scenarios.completed,
                report.scenarios.failed,
                report.elapsed.as_secs_f64()
            );
            if let Some(rate) = args.rate {
                println!("Target starts/sec:   {rate}");
            }
            let start_rate = report
                .starts
                .per_second
                .map_or_else(|| "n/a".to_owned(), |rate| format!("{rate:.2}"));
            println!("Started/sec:         {start_rate}");
            println!(
                "Start window:        {:.3} s ({start_window_method}, {} started)",
                report.starts.window.as_secs_f64(),
                report.starts.started
            );
            println!("Completed/sec:       {completed_per_second:.2}");
            if let Some(value) = mean(&report.scenarios) {
                println!("Mean scenario time:  {value:.2} ms");
            }
            println!("Steps:");
            for ((name, count), stats) in report.steps.iter().zip(&step_stats) {
                let latency =
                    mean(count).map_or_else(|| "n/a".into(), |value| format!("{value:.2} ms"));
                println!(
                    "  {name}: completed={} failed={} mean={latency}",
                    count.completed, count.failed
                );
                for p in [50, 90, 95, 99, 100] {
                    if let Some(value) = percentile(stats, p) {
                        println!("    p{p}: {value:.2} ms");
                    }
                }
                if !count.status_codes.is_empty() {
                    println!("    HTTP status codes: {:?}", count.status_codes);
                }
                if !count.failure_details.is_empty() {
                    println!("    Failure details: {:?}", count.failure_details);
                }
            }
            if args.bounded_latency {
                println!("Latency method: HDR histogram (approximate upper bounds)");
            }
            if let Some(limit) = args.max_failure_rate {
                let observed = failure_rate.map_or_else(
                    || "no completed scenarios".to_owned(),
                    |value| format!("{value:.2}%"),
                );
                println!(
                    "Scenario failure rate threshold: {observed} / {limit:.2}% — {}",
                    if failure_passed == Some(true) {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                );
            }
            if let Some(limit) = args.max_step_p95_ms {
                for (name, observed, passed) in &step_checks {
                    let observed = observed
                        .map_or_else(|| "no samples".to_owned(), |value| format!("{value:.2} ms"));
                    println!(
                        "Step {name:?} p95 threshold: {observed} / {limit:.2} ms — {}",
                        if *passed == Some(true) {
                            "PASS"
                        } else {
                            "FAIL"
                        }
                    );
                }
            }
        }
        return Ok(exit_code);
    }
    let mut headers = HeaderMap::new();
    for header in args.headers {
        let (name, value) = header
            .split_once(':')
            .ok_or("headers must have the form 'Name: value'")?;
        headers.append(
            HeaderName::from_bytes(name.trim().as_bytes())?,
            HeaderValue::from_str(value.trim())?,
        );
    }
    if let Some(user_agent) = args.user_agent {
        headers.insert(USER_AGENT, HeaderValue::from_str(&user_agent)?);
    }
    let body = args
        .body_file
        .map(std::fs::read)
        .transpose()?
        .map(Into::into);
    let mut report = run(TestPlan {
        url: args.url.expect("URL is required without --scenario"),
        requests: args.requests.unwrap_or(if args.duration.is_some() {
            usize::MAX
        } else {
            1
        }),
        concurrency: args.concurrency,
        timeout: Duration::from_secs(args.timeout),
        method: args.method,
        headers,
        body,
        duration: args
            .duration
            .map(|seconds| Duration::from_secs(seconds.get())),
        rate: args.rate,
        verbose: args.verbose,
        bounded_latency: args.bounded_latency,
        reuse_connections: !args.no_connection_reuse,
        follow_redirects: args.follow_redirects,
        max_redirects: args.max_redirects.map_or(10, NonZeroUsize::get),
    })
    .await?;
    let stats = match report.bounded_latencies.take() {
        Some(histogram) => Some(LatencyStats::from_bounded(histogram)),
        None => LatencyStats::from_samples(std::mem::take(&mut report.latencies)),
    };
    let failure_rate =
        (report.completed > 0).then(|| report.failed as f64 * 100.0 / report.completed as f64);
    let p95_ms = stats
        .as_ref()
        .and_then(|s| s.percentile(95))
        .map(|d| d.as_secs_f64() * 1000.0);
    let failure_rate_passed = args
        .max_failure_rate
        .map(|limit| failure_rate.is_some_and(|value| value <= limit));
    let p95_passed = args
        .max_p95_ms
        .map(|limit| p95_ms.is_some_and(|value| value <= limit));
    let passed = failure_rate_passed != Some(false) && p95_passed != Some(false);
    let exit_code = if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    };
    if args.json {
        let mut percentiles: std::collections::BTreeMap<String, Option<f64>> =
            [50, 90, 95, 99, 100]
                .into_iter()
                .map(|p| {
                    (
                        format!("p{p}"),
                        stats
                            .as_ref()
                            .and_then(|s| s.percentile(p))
                            .map(|d| d.as_secs_f64() * 1000.0),
                    )
                })
                .collect();
        percentiles.insert(
            "min".into(),
            stats.as_ref().map(|s| s.min().as_secs_f64() * 1000.0),
        );
        percentiles.insert("mean".into(), stats.as_ref().map(|s| s.mean_ms));
        percentiles.insert(
            "median".into(),
            stats.as_ref().map(|s| s.median().as_secs_f64() * 1000.0),
        );
        percentiles.insert(
            "max".into(),
            stats.as_ref().map(|s| s.max().as_secs_f64() * 1000.0),
        );
        let json = serde_json::json!({
            "schema_version": 1,
            "completed": report.completed,
            "failed": report.failed,
            "connections_opened": report.connections_opened,
            "failure_details": {"http": report.http_failures, "timeout": report.timeout_failures, "connection": report.connection_failures, "body": report.body_failures, "redirect_limit": report.redirect_limit_failures, "other": report.other_failures},
            "redirects_followed": report.redirects_followed,
            "response_bytes": report.bytes,
            "elapsed_seconds": report.elapsed.as_secs_f64(),
            "requests_per_second": report.completed as f64 / report.elapsed.as_secs_f64(),
            "status_codes": report.status_codes,
            "latency_ms": percentiles,
            "latency_method": if args.bounded_latency { "hdr_histogram_upper_bound_3_significant_digits" } else { "exact" },
            "thresholds": {
                "failure_rate_percent": args.max_failure_rate.map(|limit| serde_json::json!({"limit": limit, "observed": failure_rate, "passed": failure_rate_passed})),
                "p95_ms": args.max_p95_ms.map(|limit| serde_json::json!({"limit": limit, "observed": p95_ms, "passed": p95_passed})),
            },
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(exit_code);
    }
    println!("For the Horde!\nWork complete!");
    println!("{:<25}{}", "Completed requests:", report.completed);
    println!("{:<25}{}", "Failed requests:", report.failed);
    println!("{:<25}{}", "Connections opened:", report.connections_opened);
    println!("Failure details (may overlap):");
    println!("  {:<23}{}", "HTTP 4xx/5xx:", report.http_failures);
    println!("  {:<23}{}", "Timeout:", report.timeout_failures);
    println!("  {:<23}{}", "Connection:", report.connection_failures);
    println!("  {:<23}{}", "Body read:", report.body_failures);
    println!("  {:<23}{}", "Other:", report.other_failures);
    println!("{:<25}{}", "Redirects followed:", report.redirects_followed);
    println!(
        "{:<25}{}",
        "Redirect limit failures:", report.redirect_limit_failures
    );
    println!("{:<25}{}", "Response bytes:", report.bytes);
    println!("{:<25}{:.3} s", "Elapsed:", report.elapsed.as_secs_f64());
    println!(
        "{:<25}{:.2}",
        "Requests/sec:",
        report.completed as f64 / report.elapsed.as_secs_f64()
    );
    println!("Lok'tar! HTTP status codes:");
    if report.status_codes.is_empty() {
        println!("  No HTTP responses received");
    } else {
        for (code, count) in &report.status_codes {
            println!("  {code}: {count}");
        }
    }
    if args.bounded_latency {
        println!("Ready for action: latency statistics (HDR histogram, approximate upper bounds)");
    } else {
        println!("Ready for action: latency statistics (exact)");
    }
    if let Some(stats) = &stats {
        for (label, value) in [
            ("Latency min:", stats.min().as_secs_f64() * 1000.0),
            ("Latency mean:", stats.mean_ms),
            ("Latency median:", stats.median().as_secs_f64() * 1000.0),
            ("Latency max:", stats.max().as_secs_f64() * 1000.0),
        ] {
            println!("{label:<25}{value:.2} ms");
        }
    }
    for p in [50, 90, 95, 99, 100] {
        if let Some(latency) = stats.as_ref().and_then(|s| s.percentile(p)) {
            println!(
                "{:<25}{:.2} ms",
                format!("Latency p{p}:"),
                latency.as_secs_f64() * 1000.0
            );
        }
    }
    if let Some(limit) = args.max_failure_rate {
        match failure_rate {
            Some(value) => println!(
                "Failure rate threshold: {value:.2}% / {limit:.2}% — {}",
                if failure_rate_passed == Some(true) {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
            None => println!("Failure rate threshold: no completed requests / {limit:.2}% — FAIL"),
        }
    }
    if let Some(limit) = args.max_p95_ms {
        match p95_ms {
            Some(value) => println!(
                "Latency p95 threshold: {value:.2} ms / {limit:.2} ms — {}",
                if p95_passed == Some(true) {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
            None => println!("Latency p95 threshold: no samples / {limit:.2} ms — FAIL"),
        }
    }
    Ok(exit_code)
}
