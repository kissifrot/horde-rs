//! Local load generation and reporting, independent of the command-line interface.
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, Method, Url,
    header::{CONNECTION, HeaderMap, HeaderValue},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug)]
struct RedirectLimit;

impl std::fmt::Display for RedirectLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("redirect limit exceeded")
    }
}
impl std::error::Error for RedirectLimit {}

fn is_redirect_limit(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.is::<RedirectLimit>() {
            return true;
        }
        source = current.source();
    }
    false
}

/// A self-contained description of a load test.
#[derive(Clone, Debug)]
pub struct TestPlan {
    pub url: Url,
    pub requests: usize,
    pub concurrency: usize,
    pub timeout: Duration,
    pub method: Method,
    pub headers: HeaderMap,
    /// Optional reusable request body.
    pub body: Option<Bytes>,
    /// Stop starting new requests after this duration; in-flight requests finish.
    pub duration: Option<Duration>,
    /// Optional target rate for starting logical requests per second.
    pub rate: Option<u32>,
    /// Allow HTTP connections to be reused between requests.
    pub reuse_connections: bool,
    /// Whether to follow HTTP redirects.
    pub follow_redirects: bool,
    /// Maximum redirects per request when following redirects; must be nonzero.
    pub max_redirects: usize,
}

/// Aggregated results. Latencies cover complete response bodies and failed requests.
#[derive(Debug)]
pub struct Report {
    pub elapsed: Duration,
    pub completed: usize,
    pub failed: usize,
    /// Connections successfully established by the HTTP connector during the run.
    pub connections_opened: usize,
    pub http_failures: usize,
    pub timeout_failures: usize,
    pub connection_failures: usize,
    pub body_failures: usize,
    pub other_failures: usize,
    /// Redirects accepted for following, including those in failed requests.
    pub redirects_followed: usize,
    /// Requests that failed because they exceeded the redirect limit.
    pub redirect_limit_failures: usize,
    pub bytes: u64,
    /// HTTP status counts for responses whose headers were received.
    pub status_codes: BTreeMap<u16, usize>,
    pub latencies: Vec<Duration>,
}

#[derive(Default)]
struct WorkerReport {
    completed: usize,
    failed: usize,
    http_failures: usize,
    timeout_failures: usize,
    connection_failures: usize,
    body_failures: usize,
    other_failures: usize,
    bytes: u64,
    status_codes: BTreeMap<u16, usize>,
    latencies: Vec<Duration>,
}

impl Report {
    /// Calculate latency statistics, sorting the samples once.
    pub fn latency_stats(&self) -> Option<LatencyStats> {
        if self.latencies.is_empty() {
            return None;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        let mean_ms =
            sorted.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / sorted.len() as f64;
        Some(LatencyStats { sorted, mean_ms })
    }

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

/// Summary of the complete request latencies, including failed requests.
pub struct LatencyStats {
    sorted: Vec<Duration>,
    /// Arithmetic mean in milliseconds.
    pub mean_ms: f64,
}

impl LatencyStats {
    pub fn min(&self) -> Duration {
        self.sorted[0]
    }
    pub fn max(&self) -> Duration {
        self.sorted[self.sorted.len() - 1]
    }
    /// Median; for an even sample count, the midpoint of the two middle values.
    pub fn median(&self) -> Duration {
        let middle = self.sorted.len() / 2;
        if self.sorted.len().is_multiple_of(2) {
            let lower = self.sorted[middle - 1];
            lower + (self.sorted[middle] - lower) / 2
        } else {
            self.sorted[middle]
        }
    }
    /// Nearest-rank percentile.
    pub fn percentile(&self, percentile: u32) -> Option<Duration> {
        if !(1..=100).contains(&percentile) {
            return None;
        }
        let rank = (self.sorted.len() as u128 * u128::from(percentile)).div_ceil(100) as usize;
        Some(self.sorted[rank - 1])
    }
}

/// Run a plan locally. No listener or remote execution capability is started.
pub async fn run(mut plan: TestPlan) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
    if plan.requests == 0 || plan.concurrency == 0 || plan.timeout.is_zero() {
        return Err("requests, concurrency and timeout must be greater than zero".into());
    }
    if plan.duration.is_some_and(|duration| duration.is_zero()) {
        return Err("duration must be greater than zero".into());
    }
    if plan
        .rate
        .is_some_and(|rate| !(1..=1_000_000_000).contains(&rate))
    {
        return Err("rate must be between 1 and 1000000000 requests per second".into());
    }
    if plan.follow_redirects && plan.max_redirects == 0 {
        return Err("max redirects must be greater than zero when following redirects".into());
    }
    if !matches!(plan.url.scheme(), "http" | "https") {
        return Err("URL must use http or https".into());
    }
    let redirects_followed = Arc::new(AtomicUsize::new(0));
    let connections_opened = Arc::new(AtomicUsize::new(0));
    let redirect_limit_failures = Arc::new(AtomicUsize::new(0));
    let redirect_policy = if plan.follow_redirects {
        let followed = Arc::clone(&redirects_followed);
        let limit_failures = Arc::clone(&redirect_limit_failures);
        let limit = plan.max_redirects;
        reqwest::redirect::Policy::custom(move |attempt| {
            // previous includes the initial URL, which is not a redirect.
            if attempt.previous().len() > limit {
                limit_failures.fetch_add(1, Ordering::Relaxed);
                attempt.error(RedirectLimit)
            } else {
                followed.fetch_add(1, Ordering::Relaxed);
                attempt.follow()
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };
    if !plan.reuse_connections {
        plan.headers
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    let connection_counter = Arc::clone(&connections_opened);
    let mut builder = Client::builder()
        .user_agent(concat!("Horde/", env!("CARGO_PKG_VERSION")))
        .timeout(plan.timeout)
        .redirect(redirect_policy)
        .connector_layer(tower::util::MapResponseLayer::new(move |connection| {
            connection_counter.fetch_add(1, Ordering::Relaxed);
            connection
        }));
    if !plan.reuse_connections {
        builder = builder.pool_max_idle_per_host(0);
    }
    let client = builder.build()?;
    let next = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let next_start = Arc::new(Mutex::new(start));
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.concurrency.min(plan.requests) {
        let (client, plan, next, next_start) = (
            client.clone(),
            plan.clone(),
            Arc::clone(&next),
            Arc::clone(&next_start),
        );
        workers.spawn(async move {
            let mut results = WorkerReport::default();
            loop {
                if plan
                    .duration
                    .is_some_and(|duration| start.elapsed() >= duration)
                {
                    break;
                }
                // try_update prevents the counter from wrapping at usize::MAX.
                if next
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        (n < plan.requests).then(|| n + 1)
                    })
                    .is_err()
                {
                    break;
                }
                if let Some(rate) = plan.rate {
                    let scheduled = {
                        let mut next_time = next_start.lock().unwrap();
                        let scheduled = (*next_time).max(Instant::now());
                        *next_time = scheduled + Duration::from_secs_f64(1.0 / f64::from(rate));
                        scheduled
                    };
                    if plan
                        .duration
                        .is_some_and(|duration| scheduled.duration_since(start) >= duration)
                    {
                        break;
                    }
                    tokio::time::sleep_until(scheduled.into()).await;
                    if plan
                        .duration
                        .is_some_and(|duration| start.elapsed() >= duration)
                    {
                        break;
                    }
                }
                let started = Instant::now();
                let mut bytes = 0u64;
                let mut failed = false;
                let mut status_code = None;
                let mut http_failure = false;
                let mut timeout_failure = false;
                let mut connection_failure = false;
                let mut body_failure = false;
                let mut other_failure = false;
                let mut request = client
                    .request(plan.method.clone(), plan.url.clone())
                    .headers(plan.headers.clone());
                if let Some(body) = &plan.body {
                    request = request.body(body.clone());
                }
                match request.send().await {
                    Ok(response) => {
                        status_code = Some(response.status().as_u16());
                        http_failure = response.status().is_client_error()
                            || response.status().is_server_error();
                        failed |= http_failure;
                        let mut body = response.bytes_stream();
                        while let Some(chunk) = body.next().await {
                            match chunk {
                                Ok(chunk) => bytes = bytes.saturating_add(chunk.len() as u64),
                                Err(error) => {
                                    failed = true;
                                    body_failure = true;
                                    timeout_failure = error.is_timeout();
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        failed = true;
                        timeout_failure = error.is_timeout();
                        connection_failure = error.is_connect();
                        other_failure =
                            !timeout_failure && !connection_failure && !is_redirect_limit(&error);
                    }
                }
                let latency = started.elapsed();
                results.completed += 1;
                results.failed += usize::from(failed);
                results.http_failures += usize::from(http_failure);
                results.timeout_failures += usize::from(timeout_failure);
                results.connection_failures += usize::from(connection_failure);
                results.body_failures += usize::from(body_failure);
                results.other_failures += usize::from(other_failure);
                results.bytes = results.bytes.saturating_add(bytes);
                if let Some(code) = status_code {
                    *results.status_codes.entry(code).or_default() += 1;
                }
                results.latencies.push(latency);
            }
            results
        });
    }
    let mut report = Report {
        elapsed: Duration::ZERO,
        completed: 0,
        failed: 0,
        connections_opened: 0,
        http_failures: 0,
        timeout_failures: 0,
        connection_failures: 0,
        body_failures: 0,
        other_failures: 0,
        redirects_followed: 0,
        redirect_limit_failures: 0,
        bytes: 0,
        status_codes: BTreeMap::new(),
        latencies: Vec::new(),
    };
    while let Some(worker) = workers.join_next().await {
        let worker = worker?;
        report.completed += worker.completed;
        report.failed += worker.failed;
        report.http_failures += worker.http_failures;
        report.timeout_failures += worker.timeout_failures;
        report.connection_failures += worker.connection_failures;
        report.body_failures += worker.body_failures;
        report.other_failures += worker.other_failures;
        report.bytes = report.bytes.saturating_add(worker.bytes);
        for (code, count) in worker.status_codes {
            *report.status_codes.entry(code).or_default() += count;
        }
        report.latencies.extend(worker.latencies);
    }
    report.elapsed = start.elapsed();
    report.redirects_followed = redirects_followed.load(Ordering::Relaxed);
    report.redirect_limit_failures = redirect_limit_failures.load(Ordering::Relaxed);
    report.connections_opened = connections_opened.load(Ordering::Relaxed);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nearest_rank_percentiles() {
        let mut report = Report {
            elapsed: Duration::ZERO,
            completed: 3,
            failed: 0,
            connections_opened: 0,
            http_failures: 0,
            timeout_failures: 0,
            connection_failures: 0,
            body_failures: 0,
            other_failures: 0,
            redirects_followed: 0,
            redirect_limit_failures: 0,
            bytes: 0,
            status_codes: BTreeMap::new(),
            latencies: vec![
                Duration::from_millis(30),
                Duration::from_millis(10),
                Duration::from_millis(20),
            ],
        };
        assert_eq!(report.percentile(50), Some(Duration::from_millis(20)));
        assert_eq!(report.percentile(99), Some(Duration::from_millis(30)));
        assert_eq!(report.percentile(0), None);
        let stats = report.latency_stats().unwrap();
        assert_eq!(stats.min(), Duration::from_millis(10));
        assert_eq!(stats.max(), Duration::from_millis(30));
        assert_eq!(stats.median(), Duration::from_millis(20));
        assert_eq!(stats.mean_ms, 20.0);
        assert_eq!(stats.percentile(99), report.percentile(99));
        report.latencies.push(Duration::from_millis(40));
        assert_eq!(
            report.latency_stats().unwrap().median(),
            Duration::from_millis(25)
        );
        report.latencies.clear();
        assert!(report.latency_stats().is_none());
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
            body: None,
            duration: None,
            rate: None,
            reuse_connections: true,
            follow_redirects: false,
            max_redirects: 10,
        };
        assert!(run(plan).await.is_err());
    }
}
