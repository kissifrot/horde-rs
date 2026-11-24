#[cfg(feature = "distributed-transport")]
mod distributed_cli;
mod live_report;
mod signals;
mod text_report;
use clap::Parser;
use horde_rs::scenario::{
    Counts, Scenario, ScenarioOptions, StartWindow, run_scenario_with_progress,
};
use horde_rs::{Cancellation, LatencyStats, TestPlan, run_with_progress};
use live_report::LiveReport;
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
use text_report::TextReport;

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
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    after_help = "Zug zug! Only test services you are authorized to load test."
)]
struct Args {
    #[cfg(feature = "distributed-transport")]
    #[command(subcommand)]
    distributed: Option<distributed_cli::Command>,
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
    #[cfg(feature = "distributed-transport")]
    if let Some(command) = args.distributed {
        return distributed_cli::run(command).await;
    }
    let cancellation = Cancellation::new();
    let reporting = LiveReport::new(
        !args.json && !args.validate,
        args.verbose,
        args.scenario.is_some(),
    );
    let interrupts = signals::install(cancellation.clone(), reporting.live)?;
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
            if interrupts.borrow().is_some() {
                return Ok(signals::report(&interrupts, args.json));
            }
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
        let run = run_scenario_with_progress(
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
            cancellation.clone(),
            reporting.progress.as_ref(),
        );
        let result = reporting.monitor(&interrupts, run).await?;
        let Some(mut report) = result else {
            return Ok(reporting.interrupted(&interrupts, args.json, args.bounded_latency)?);
        };
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
        let thresholds_failed = failure_passed == Some(false) || step_passed == Some(false);
        let exit_code = if thresholds_failed {
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
            let mut output = TextReport::standalone(thresholds_failed, report.scenarios.failed > 0);
            output.section("Scenarios");
            output.field("Scenario:", format_args!("{:?}", report.name));
            output.field("Completed scenarios:", report.scenarios.completed);
            output.field("Failed scenarios:", report.scenarios.failed);
            output.field(
                "Elapsed:",
                format_args!("{:.3} s", report.elapsed.as_secs_f64()),
            );
            output.field("Completed/sec:", format_args!("{completed_per_second:.2}"));
            if let Some(rate) = args.rate {
                output.field("Target starts/sec:", rate);
            }
            let start_rate = report
                .starts
                .per_second
                .map_or_else(|| "n/a".to_owned(), |rate| format!("{rate:.2}"));
            output.field("Started/sec:", start_rate);
            output.field(
                "Start window:",
                format_args!(
                    "{:.3} s ({start_window_method}, {} started)",
                    report.starts.window.as_secs_f64(),
                    report.starts.started
                ),
            );

            output.section(if args.bounded_latency {
                "Iteration duration (HDR histogram, approximate upper bounds)"
            } else {
                "Iteration duration (exact)"
            });
            if let Some(value) = mean(&report.scenarios) {
                for p in [50, 95, 99] {
                    if let Some(duration) = percentile(&scenario_stats, p) {
                        output.field(&format!("Duration p{p}:"), format_args!("{duration:.2} ms"));
                    }
                }
                output.field("Mean scenario time:", format_args!("{value:.2} ms"));
            } else {
                output.line(format_args!("No samples"));
            }

            output.section("Steps");
            for ((name, count), stats) in report.steps.iter().zip(&step_stats) {
                let skipped = report.scenarios.completed - count.completed;
                output.line(format_args!(
                    "  {name:?}  |  Completed: {}  Failed: {}  Skipped: {skipped}",
                    count.completed, count.failed,
                ));
                let latency = mean(count).map(|mean| {
                    std::iter::once(format!("mean {mean:.2} ms"))
                        .chain([50, 90, 95, 99, 100].into_iter().filter_map(|p| {
                            percentile(stats, p).map(|value| format!("p{p} {value:.2} ms"))
                        }))
                        .collect::<Vec<_>>()
                        .join("  ")
                });
                output.line(format_args!(
                    "    Latency: {}",
                    latency.as_deref().unwrap_or("no samples")
                ));
                if !count.status_codes.is_empty() {
                    let codes = count
                        .status_codes
                        .iter()
                        .map(|(code, count)| format!("{code}: {count}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    output.line(format_args!("    Lok'tar! HTTP status codes: {codes}"));
                }
                if !count.failure_details.is_empty() {
                    let failures = count
                        .failure_details
                        .iter()
                        .map(|(category, count)| format!("{category}: {count}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    output.line(format_args!(
                        "    Failure details (may overlap): {failures}"
                    ));
                }
            }
            if args.max_failure_rate.is_some() || args.max_step_p95_ms.is_some() {
                output.section("Thresholds");
            }
            if let Some(limit) = args.max_failure_rate {
                let observed = failure_rate.map_or_else(
                    || "no completed scenarios".to_owned(),
                    |value| format!("{value:.2}%"),
                );
                let verdict = output.verdict(failure_passed == Some(true));
                output.field(
                    "Scenario failure rate:",
                    format_args!("{observed} / {limit:.2}% — {verdict}"),
                );
            }
            if let Some(limit) = args.max_step_p95_ms {
                for (name, observed, passed) in &step_checks {
                    let observed = observed
                        .map_or_else(|| "no samples".to_owned(), |value| format!("{value:.2} ms"));
                    let verdict = output.verdict(*passed == Some(true));
                    output.line(format_args!(
                        "Step {name:?} p95: {observed} / {limit:.2} ms — {verdict}",
                    ));
                }
            }
            output.print()?;
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
    let run = run_with_progress(
        TestPlan {
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
        },
        cancellation,
        reporting.progress.as_ref(),
    );
    let result = reporting.monitor(&interrupts, run).await?;
    let Some(mut report) = result else {
        return Ok(reporting.interrupted(&interrupts, args.json, args.bounded_latency)?);
    };
    let stats = report.take_latency_stats();
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
    let mut output = TextReport::standalone(!passed, report.failed > 0);
    output.section("Requests");
    output.field("Completed requests:", report.completed);
    output.field("Failed requests:", report.failed);
    let elapsed = report.elapsed.as_secs_f64();
    output.field("Elapsed:", format_args!("{elapsed:.3} s"));
    output.field(
        "Requests/sec:",
        format_args!("{:.2}", report.completed as f64 / elapsed),
    );

    output.section(if args.bounded_latency {
        "Latency (HDR histogram, approximate upper bounds)"
    } else {
        "Latency (exact)"
    });
    if let Some(stats) = &stats {
        for p in [50, 90, 95, 99, 100] {
            if let Some(latency) = stats.percentile(p) {
                output.field(
                    &format!("Latency p{p}:"),
                    format_args!("{:.2} ms", latency.as_secs_f64() * 1000.0),
                );
            }
        }
        for (label, value) in [
            ("Latency min:", stats.min().as_secs_f64() * 1000.0),
            ("Latency mean:", stats.mean_ms),
            ("Latency median:", stats.median().as_secs_f64() * 1000.0),
            ("Latency max:", stats.max().as_secs_f64() * 1000.0),
        ] {
            output.field(label, format_args!("{value:.2} ms"));
        }
    } else {
        output.line(format_args!("No samples"));
    }

    output.section("HTTP and connections");
    output.field("Connections opened:", report.connections_opened);
    output.field("Redirects followed:", report.redirects_followed);
    output.field("Redirect limit failures:", report.redirect_limit_failures);
    output.field("Response bytes:", report.bytes);
    if report.status_codes.is_empty() {
        output.line(format_args!("No HTTP responses received"));
    } else {
        output.line(format_args!("Lok'tar! HTTP status codes:"));
        for (code, count) in &report.status_codes {
            output.line(format_args!("  {code}: {count}"));
        }
    }
    output.section("Failure details (may overlap)");
    for (label, count) in [
        ("HTTP 4xx/5xx:", report.http_failures),
        ("Timeout:", report.timeout_failures),
        ("Connection:", report.connection_failures),
        ("Body read:", report.body_failures),
        ("Other:", report.other_failures),
    ] {
        output.line(format_args!("  {label:<23}{count}"));
    }

    if args.max_failure_rate.is_some() || args.max_p95_ms.is_some() {
        output.section("Thresholds");
    }
    for (label, limit, observed, unit, missing, passed) in [
        (
            "Failure rate:",
            args.max_failure_rate,
            failure_rate,
            "%",
            "no completed requests",
            failure_rate_passed,
        ),
        (
            "Latency p95:",
            args.max_p95_ms,
            p95_ms,
            " ms",
            "no samples",
            p95_passed,
        ),
    ] {
        let Some(limit) = limit else { continue };
        let observed =
            observed.map_or_else(|| missing.to_owned(), |value| format!("{value:.2}{unit}"));
        let verdict = output.verdict(passed == Some(true));
        output.field(
            label,
            format_args!("{observed} / {limit:.2}{unit} — {verdict}"),
        );
    }
    output.print()?;
    Ok(exit_code)
}
