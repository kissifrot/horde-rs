use clap::Parser;
use horde_rs::{TestPlan, run};
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

/// For the Horde! Run an HTTP load test from this machine.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Zug zug! Only test services you are authorized to load test."
)]
struct Args {
    /// HTTP or HTTPS target URL.
    url: Url,
    /// Total number of requests.
    #[arg(short = 'n', long)]
    requests: Option<usize>,
    /// Maximum number of concurrent requests.
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
    /// Stop starting requests after this many seconds.
    #[arg(long)]
    duration: Option<NonZeroU64>,
    /// Target rate in logical requests per second (1 to 1000000000).
    #[arg(long)]
    rate: Option<u32>,
    /// Disable HTTP connection reuse.
    #[arg(long)]
    no_connection_reuse: bool,
    /// Maximum failed requests as a percentage (0 to 100).
    #[arg(long, allow_hyphen_values = true)]
    max_failure_rate: Option<f64>,
    /// Maximum p95 request latency in milliseconds.
    #[arg(long, allow_hyphen_values = true)]
    max_p95_ms: Option<f64>,
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
    let report = run(TestPlan {
        url: args.url,
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
        reuse_connections: !args.no_connection_reuse,
        follow_redirects: args.follow_redirects,
        max_redirects: args.max_redirects.map_or(10, NonZeroUsize::get),
    })
    .await?;
    let stats = report.latency_stats();
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
    println!("Ready for action: latency statistics");
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
