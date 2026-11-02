//! Local load generation and reporting, independent of the command-line interface.
use futures_util::StreamExt;
use reqwest::{Client, Method, Url, header::HeaderMap};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// A self-contained description of a load test.
#[derive(Clone, Debug)]
pub struct TestPlan {
    pub url: Url,
    pub requests: usize,
    pub concurrency: usize,
    pub timeout: Duration,
    pub method: Method,
    pub headers: HeaderMap,
}

/// Aggregated results. Latencies cover complete response bodies and failed requests.
#[derive(Debug)]
pub struct Report {
    pub elapsed: Duration,
    pub completed: usize,
    pub failed: usize,
    pub bytes: u64,
    pub latencies: Vec<Duration>,
}

impl Report {
    /// Nearest-rank percentile, with ranks starting at one.
    pub fn percentile(&self, percentile: u32) -> Option<Duration> {
        if self.latencies.is_empty() || !(1..=100).contains(&percentile) {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        let rank = (sorted.len() as u128 * u128::from(percentile)).div_ceil(100) as usize;
        Some(sorted[rank - 1])
    }
}

/// Run a plan locally. No listener or remote execution capability is started.
pub async fn run(plan: TestPlan) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
    if plan.requests == 0 || plan.concurrency == 0 || plan.timeout.is_zero() {
        return Err("requests, concurrency and timeout must be greater than zero".into());
    }
    if !matches!(plan.url.scheme(), "http" | "https") {
        return Err("URL must use http or https".into());
    }
    let client = Client::builder()
        .timeout(plan.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let next = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.concurrency.min(plan.requests) {
        let (client, plan, next) = (client.clone(), plan.clone(), Arc::clone(&next));
        workers.spawn(async move {
            let mut results = Vec::new();
            loop {
                // try_update prevents the counter from wrapping at usize::MAX.
                if next
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        (n < plan.requests).then(|| n + 1)
                    })
                    .is_err()
                {
                    break;
                }
                let started = Instant::now();
                let mut bytes = 0u64;
                let mut failed = false;
                match client
                    .request(plan.method.clone(), plan.url.clone())
                    .headers(plan.headers.clone())
                    .send()
                    .await
                {
                    Ok(response) => {
                        failed |= response.status().is_client_error()
                            || response.status().is_server_error();
                        let mut body = response.bytes_stream();
                        while let Some(chunk) = body.next().await {
                            match chunk {
                                Ok(chunk) => bytes = bytes.saturating_add(chunk.len() as u64),
                                Err(_) => {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => failed = true,
                }
                results.push((started.elapsed(), failed, bytes));
            }
            results
        });
    }
    let mut report = Report {
        elapsed: Duration::ZERO,
        completed: 0,
        failed: 0,
        bytes: 0,
        latencies: Vec::new(),
    };
    while let Some(worker) = workers.join_next().await {
        for (latency, failed, bytes) in worker? {
            report.completed += 1;
            report.failed += usize::from(failed);
            report.bytes = report.bytes.saturating_add(bytes);
            report.latencies.push(latency);
        }
    }
    report.elapsed = start.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nearest_rank_percentiles() {
        let report = Report {
            elapsed: Duration::ZERO,
            completed: 3,
            failed: 0,
            bytes: 0,
            latencies: vec![
                Duration::from_millis(30),
                Duration::from_millis(10),
                Duration::from_millis(20),
            ],
        };
        assert_eq!(report.percentile(50), Some(Duration::from_millis(20)));
        assert_eq!(report.percentile(99), Some(Duration::from_millis(30)));
        assert_eq!(report.percentile(0), None);
    }
    #[tokio::test]
    async fn rejects_zero_requests() {
        let plan = TestPlan {
            url: "http://localhost".parse().unwrap(),
            requests: 0,
            concurrency: 1,
            timeout: Duration::from_secs(1),
            method: Method::GET,
            headers: HeaderMap::new(),
        };
        assert!(run(plan).await.is_err());
    }
}
