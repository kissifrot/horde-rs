//! Local load generation and reporting, independent of the command-line interface.
mod latency;
pub mod scenario;
use bytes::Bytes;
use futures_util::StreamExt;
pub use latency::BoundedLatency;
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
    /// Log request progress to stderr; logging can affect benchmark results.
    pub verbose: bool,
    /// Use bounded-memory approximate latency percentiles.
    pub bounded_latency: bool,
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
    /// Present when bounded-memory latency collection was selected and recorded samples exist.
    pub bounded_latencies: Option<BoundedLatency>,
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
    bounded_latencies: Option<BoundedLatency>,
    latency_overflow: bool,
}

impl Report {
    /// Calculate latency statistics, sorting the samples once.
    pub fn latency_stats(&self) -> Option<LatencyStats> {
        if let Some(histogram) = &self.bounded_latencies {
            Some(LatencyStats::from_bounded(histogram.clone()))
        } else {
            LatencyStats::from_samples(self.latencies.clone())
        }
    }

    /// Nearest-rank percentile, with ranks starting at one.
    pub fn percentile(&self, percentile: u32) -> Option<Duration> {
        self.latency_stats()?.percentile(percentile)
    }
}

/// Summary of the complete request latencies, including failed requests.
pub struct LatencyStats {
    sorted: Vec<Duration>,
    bounded: Option<BoundedLatency>,
    /// Arithmetic mean in milliseconds.
    pub mean_ms: f64,
}

impl LatencyStats {
    /// Sort owned samples in place, without allocating a second sample vector.
    pub fn from_samples(mut sorted: Vec<Duration>) -> Option<Self> {
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_unstable();
        let mean_ms =
            sorted.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / sorted.len() as f64;
        Some(Self {
            sorted,
            bounded: None,
            mean_ms,
        })
    }

    /// Use an owned bounded histogram without copying it.
    pub fn from_bounded(bounded: BoundedLatency) -> Self {
        let mean_ms = bounded.mean_ms();
        Self {
            sorted: Vec::new(),
            bounded: Some(bounded),
            mean_ms,
        }
    }

    pub fn min(&self) -> Duration {
        self.bounded
            .as_ref()
            .map_or_else(|| self.sorted[0], BoundedLatency::min)
    }
    pub fn max(&self) -> Duration {
        self.bounded
            .as_ref()
            .map_or_else(|| self.sorted[self.sorted.len() - 1], BoundedLatency::max)
    }
    /// Median; for an even sample count, the midpoint of the two middle values.
    pub fn median(&self) -> Duration {
        if let Some(histogram) = &self.bounded {
            let count = histogram.len();
            let lower = histogram.rank(count.div_ceil(2));
            let upper = histogram.rank(count / 2 + 1);
            return lower + (upper - lower) / 2;
        }
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
        if let Some(histogram) = &self.bounded {
            let rank = (u128::from(histogram.len()) * u128::from(percentile)).div_ceil(100) as u64;
            return Some(histogram.rank(rank));
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
    for worker_id in 1..=plan.concurrency.min(plan.requests) {
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
                let request_id = match next
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        (n < plan.requests).then(|| n + 1)
                    })
                {
                    Ok(index) => index + 1,
                    Err(_) => break,
                };
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
                if plan.verbose {
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [worker {worker_id} request {request_id}] started", started.duration_since(start).as_secs_f64());
                }
                let mut bytes = 0u64;
                let mut failed = false;
                let mut status_code = None;
                let mut http_failure = false;
                let mut timeout_failure = false;
                let mut connection_failure = false;
                let mut body_failure = false;
                let mut other_failure = false;
                let mut redirect_limit_failure = false;
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
                        redirect_limit_failure = (plan.verbose || (!timeout_failure && !connection_failure))
                            && is_redirect_limit(&error);
                        other_failure =
                            !timeout_failure && !connection_failure && !redirect_limit_failure;
                    }
                }
                let latency = started.elapsed();
                if plan.verbose {
                    use std::io::Write;
                    let status = status_code.map_or_else(|| "none".to_owned(), |code| code.to_string());
                    let errors = [(http_failure, "http"), (timeout_failure, "timeout"), (connection_failure, "connection"), (body_failure, "body"), (redirect_limit_failure, "redirect_limit"), (other_failure, "other")];
                    let mut stderr = std::io::stderr().lock();
                    let _ = write!(stderr, "[+{:.3}s] [worker {worker_id} request {request_id}] completed status={status} bytes={bytes} latency_ms={:.2}", started.duration_since(start).as_secs_f64() + latency.as_secs_f64(), latency.as_secs_f64() * 1000.0);
                    if failed {
                        let _ = write!(stderr, " errors=");
                        let mut separator = "";
                        for (_, name) in errors.into_iter().filter(|(active, _)| *active) {
                            let _ = write!(stderr, "{separator}{name}");
                            separator = ",";
                        }
                    }
                    let _ = writeln!(stderr);
                }
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
                if plan.bounded_latency {
                    let histogram = results.bounded_latencies.get_or_insert_with(BoundedLatency::new);
                    if histogram.record(latency).is_err() {
                        results.latency_overflow = true;
                    }
                } else {
                    results.latencies.push(latency);
                }
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
        bounded_latencies: None,
    };
    while let Some(worker) = workers.join_next().await {
        let worker = worker?;
        if worker.latency_overflow {
            return Err("latency exceeds bounded histogram range or capacity".into());
        }
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
        if report.latencies.is_empty() {
            report.latencies = worker.latencies;
        } else {
            report.latencies.extend(worker.latencies);
        }
        if let Some(histogram) = worker.bounded_latencies {
            if let Some(current) = &mut report.bounded_latencies {
                current.merge(&histogram)?;
            } else {
                report.bounded_latencies = Some(histogram);
            }
        }
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
            bounded_latencies: None,
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
        let owned = LatencyStats::from_samples(report.latencies.clone()).unwrap();
        assert_eq!(owned.percentile(50), report.percentile(50));
        assert_eq!(owned.percentile(99), report.percentile(99));
        report.latencies.push(Duration::from_millis(40));
        assert_eq!(
            report.latency_stats().unwrap().median(),
            Duration::from_millis(25)
        );
        report.latencies.clear();
        assert!(report.latency_stats().is_none());
        assert!(LatencyStats::from_samples(Vec::new()).is_none());
    }

    #[test]
    fn bounded_percentiles_are_upper_bounds_and_merge_correctly() {
        let values: Vec<_> = (0..10_000u64)
            .map(|n| Duration::from_nanos(n * n * 1_234 + 1))
            .collect();
        let exact = LatencyStats::from_samples(values.clone()).unwrap();
        let mut left = BoundedLatency::new();
        let mut right = BoundedLatency::new();
        for (index, value) in values.into_iter().enumerate() {
            if index % 2 == 0 {
                left.record(value).unwrap();
            } else {
                right.record(value).unwrap();
            }
        }
        left.merge(&right).unwrap();
        let bounded = LatencyStats::from_bounded(left);
        assert_eq!(bounded.min(), exact.min());
        assert_eq!(bounded.max(), exact.max());
        for p in [1, 50, 90, 95, 99, 100] {
            let actual = exact.percentile(p).unwrap().as_nanos();
            let estimate = bounded.percentile(p).unwrap().as_nanos();
            assert!(estimate >= actual);
            assert!(
                estimate - actual <= actual / 1000 + 1,
                "p{p}: {actual} vs {estimate}"
            );
        }
        assert!(bounded.median() >= exact.median());
        assert!(bounded.percentile(0).is_none());
    }

    #[test]
    fn bounded_histogram_handles_extremes_and_rejects_overflow() {
        let mut histogram = BoundedLatency::new();
        histogram.record(Duration::ZERO).unwrap();
        histogram.record(Duration::from_nanos(u64::MAX)).unwrap();
        assert!(histogram.record(Duration::from_secs(u64::MAX)).is_err());
        let stats = LatencyStats::from_bounded(histogram);
        assert_eq!(stats.percentile(50), Some(Duration::ZERO));
        assert_eq!(stats.percentile(100), Some(Duration::from_nanos(u64::MAX)));
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
            verbose: false,
            bounded_latency: false,
            reuse_connections: true,
            follow_redirects: false,
            max_redirects: 10,
        };
        assert!(run(plan).await.is_err());
    }
}
