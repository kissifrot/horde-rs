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
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();
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
        follow_redirects: args.follow_redirects,
        max_redirects: args.max_redirects.map_or(10, NonZeroUsize::get),
    })
    .await?;
    let stats = report.latency_stats();
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
            "failure_details": {"http": report.http_failures, "timeout": report.timeout_failures, "connection": report.connection_failures, "body": report.body_failures, "redirect_limit": report.redirect_limit_failures, "other": report.other_failures},
            "redirects_followed": report.redirects_followed,
            "response_bytes": report.bytes,
            "elapsed_seconds": report.elapsed.as_secs_f64(),
            "requests_per_second": report.completed as f64 / report.elapsed.as_secs_f64(),
            "status_codes": report.status_codes,
            "latency_ms": percentiles,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }
    println!("For the Horde!\nWork complete!");
    println!("{:<25}{}", "Completed requests:", report.completed);
    println!("{:<25}{}", "Failed requests:", report.failed);
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
    Ok(())
}
