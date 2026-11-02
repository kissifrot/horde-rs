use clap::Parser;
use horde_rs::{TestPlan, run};
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{error::Error, time::Duration};

/// For the Horde! Run an HTTP load test from this machine.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// HTTP or HTTPS target URL.
    url: Url,
    /// Total number of requests.
    #[arg(short = 'n', long, default_value_t = 1)]
    requests: usize,
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
    let report = run(TestPlan {
        url: args.url,
        requests: args.requests,
        concurrency: args.concurrency,
        timeout: Duration::from_secs(args.timeout),
        method: args.method,
        headers,
    })
    .await?;
    println!(
        "For the Horde!\nCompleted requests: {}\nFailed requests:    {}\nResponse bytes:    {}\nElapsed:           {:.3} s\nRequests/sec:      {:.2}",
        report.completed,
        report.failed,
        report.bytes,
        report.elapsed.as_secs_f64(),
        report.completed as f64 / report.elapsed.as_secs_f64()
    );
    for p in [50, 90, 95, 99, 100] {
        if let Some(latency) = report.percentile(p) {
            println!(
                "Latency p{p:<3}:      {:.2} ms",
                latency.as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}
