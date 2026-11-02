//! Local HTTP journeys with isolated virtual-user sessions.
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    error::Error,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

type RunError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioFile {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    version: u64,
    name: String,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    steps: Vec<StepFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    name: String,
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body: Option<String>,
    #[serde(default)]
    wait_ms: u64,
}

fn default_method() -> String {
    "GET".into()
}

fn valid_variable_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn expand(input: &str, variables: &BTreeMap<String, String>) -> Result<String, RunError> {
    let mut rest = input;
    let mut output = String::with_capacity(input.len());
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let expression = &rest[start + 2..];
        let end = expression.find("}}").ok_or("unclosed scenario variable")?;
        let name = &expression[..end];
        if !valid_variable_name(name) {
            return Err("invalid scenario variable name".into());
        }
        let value = variables
            .get(name)
            .ok_or_else(|| format!("undefined scenario variable: {name}"))?;
        output.push_str(value);
        rest = &expression[end + 2..];
    }
    output.push_str(rest);
    Ok(output)
}

/// A validated scenario; every URL must be absolute HTTP or HTTPS.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    steps: Vec<Step>,
}

#[derive(Clone, Debug)]
struct Step {
    name: String,
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<Bytes>,
    wait: Duration,
}

impl Scenario {
    /// Parse and validate a JSON scenario before making any requests.
    pub fn from_json(input: &[u8]) -> Result<Self, RunError> {
        let document: serde_json::Value = serde_json::from_slice(input)?;
        match document.get("version").and_then(serde_json::Value::as_u64) {
            Some(1) => {}
            Some(version) => {
                return Err(
                    format!("unsupported scenario version: {version} (supported: 1)").into(),
                );
            }
            None => return Err("scenario version must be the integer 1".into()),
        }
        static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
        let validator = VALIDATOR.get_or_init(|| {
            let schema = serde_json::from_str(include_str!("../schemas/scenario-v1.schema.json"))
                .expect("embedded scenario schema is valid JSON");
            jsonschema::validator_for(&schema).expect("embedded scenario schema is valid")
        });
        validator
            .validate(&document)
            .map_err(|error| format!("invalid scenario at {}: {error}", error.instance_path()))?;
        let file: ScenarioFile = serde_json::from_slice(input)?;
        debug_assert_eq!(file.version, 1);
        if file.name.trim().is_empty() || file.steps.is_empty() {
            return Err("scenario name and steps must not be empty".into());
        }
        if file.variables.keys().any(|name| !valid_variable_name(name)) {
            return Err("variable names must contain only letters, digits or underscores".into());
        }
        let mut names = std::collections::HashSet::new();
        let mut steps = Vec::with_capacity(file.steps.len());
        for step in file.steps {
            if step.name.trim().is_empty() || !names.insert(step.name.clone()) {
                return Err("step names must be nonempty and unique".into());
            }
            let url: Url = expand(&step.url, &file.variables)?.parse()?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("step URLs must be absolute HTTP or HTTPS URLs".into());
            }
            let mut headers = HeaderMap::new();
            for (key, value) in step.headers {
                headers.insert(
                    HeaderName::from_bytes(key.as_bytes())?,
                    HeaderValue::from_str(&expand(&value, &file.variables)?)?,
                );
            }
            steps.push(Step {
                name: step.name,
                url,
                method: step.method.parse()?,
                headers,
                body: step
                    .body
                    .map(|body| expand(&body, &file.variables))
                    .transpose()?
                    .map(Bytes::from),
                wait: Duration::from_millis(step.wait_ms),
            });
        }
        Ok(Self {
            name: file.name,
            steps,
        })
    }
}

/// Results for a step or scenario; elapsed time for a scenario includes waits.
#[derive(Default, Debug, Clone)]
pub struct Counts {
    pub completed: usize,
    pub failed: usize,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct ScenarioReport {
    pub name: String,
    pub elapsed: Duration,
    pub scenarios: Counts,
    pub steps: Vec<(String, Counts)>,
}

/// Run a fixed number of scenario iterations, keeping one cookie jar per virtual user.
pub async fn run_scenario(
    scenario: Scenario,
    iterations: usize,
    users: usize,
    timeout: Duration,
) -> Result<ScenarioReport, RunError> {
    run_scenario_with_options(scenario, iterations, users, timeout, false).await
}

/// Run a scenario and optionally log progress to stderr.
pub async fn run_scenario_with_options(
    scenario: Scenario,
    iterations: usize,
    users: usize,
    timeout: Duration,
    verbose: bool,
) -> Result<ScenarioReport, RunError> {
    run_scenario_with_redirects(scenario, iterations, users, timeout, verbose, None).await
}

/// Run a scenario with an optional maximum number of redirects per step.
pub async fn run_scenario_with_redirects(
    scenario: Scenario,
    iterations: usize,
    users: usize,
    timeout: Duration,
    verbose: bool,
    max_redirects: Option<usize>,
) -> Result<ScenarioReport, RunError> {
    if iterations == 0 || users == 0 || timeout.is_zero() {
        return Err("iterations, users and timeout must be greater than zero".into());
    }
    if max_redirects == Some(0) {
        return Err("max redirects must be greater than zero".into());
    }
    let scenario = Arc::new(scenario);
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    let started = Instant::now();
    for user_id in 1..=users.min(iterations) {
        let scenario = Arc::clone(&scenario);
        let next = Arc::clone(&next);
        let client = Client::builder()
            .cookie_store(true)
            .timeout(timeout)
            .user_agent(concat!("Horde/", env!("CARGO_PKG_VERSION")))
            .redirect(match max_redirects {
                Some(limit) => reqwest::redirect::Policy::limited(limit),
                None => reqwest::redirect::Policy::none(),
            })
            .build()?;
        tasks.spawn(async move {
            let mut totals = Counts::default();
            let mut steps = vec![Counts::default(); scenario.steps.len()];
            while let Ok(index) = next.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < iterations).then(|| n + 1)
                }) {
                let iteration = index + 1;
                let run_started = Instant::now();
                if verbose {
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] started", started.elapsed().as_secs_f64());
                }
                let mut run_failed = false;
                for (index, step) in scenario.steps.iter().enumerate() {
                    if !step.wait.is_zero() {
                        if verbose {
                            use std::io::Write;
                            let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] waiting {} ms before step {:?}", started.elapsed().as_secs_f64(), step.wait.as_millis(), step.name);
                        }
                        tokio::time::sleep(step.wait).await;
                    }
                    if verbose {
                        use std::io::Write;
                        let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] step {:?} started", started.elapsed().as_secs_f64(), step.name);
                    }
                    let step_started = Instant::now();
                    let mut request = client
                        .request(step.method.clone(), step.url.clone())
                        .headers(step.headers.clone());
                    if let Some(body) = &step.body {
                        request = request.body(body.clone());
                    }
                    let mut status = None;
                    let mut bytes = 0u64;
                    let mut http_error = false;
                    let mut timeout_error = false;
                    let mut connection_error = false;
                    let mut body_error = false;
                    let mut redirect_error = false;
                    let mut other_error = false;
                    let failed = match request.send().await {
                        Ok(response) => {
                            status = Some(response.status().as_u16());
                            http_error = response.status().is_client_error()
                                || response.status().is_server_error();
                            let mut body = response.bytes_stream();
                            while let Some(chunk) = body.next().await {
                                match chunk {
                                    Ok(chunk) => { if verbose { bytes = bytes.saturating_add(chunk.len() as u64); } }
                                    Err(error) => { body_error = true; timeout_error = error.is_timeout(); break; }
                                }
                            }
                            http_error || body_error
                        }
                        Err(error) => {
                            timeout_error = error.is_timeout();
                            connection_error = error.is_connect();
                            redirect_error = error.is_redirect();
                            other_error = !timeout_error && !connection_error && !redirect_error;
                            true
                        },
                    };
                    let latency = step_started.elapsed();
                    if verbose {
                        use std::io::Write;
                        let mut stderr = std::io::stderr().lock();
                        let status = status.map_or_else(|| "none".to_owned(), |code| code.to_string());
                        let _ = write!(stderr, "[+{:.3}s] [user {user_id} scenario {iteration}] step {:?} completed status={status} bytes={bytes} latency_ms={:.2}", started.elapsed().as_secs_f64(), step.name, latency.as_secs_f64() * 1000.0);
                        if failed {
                            let _ = write!(stderr, " errors=");
                            let mut separator = "";
                            for (_, label) in [(http_error, "http"), (timeout_error, "timeout"), (connection_error, "connection"), (body_error, "body"), (redirect_error, "redirect"), (other_error, "other")].into_iter().filter(|(active, _)| *active) {
                                let _ = write!(stderr, "{separator}{label}");
                                separator = ",";
                            }
                        }
                        let _ = writeln!(stderr);
                    }
                    steps[index].completed += 1;
                    steps[index].failed += usize::from(failed);
                    steps[index].elapsed =
                        steps[index].elapsed.saturating_add(latency);
                    if failed {
                        run_failed = true;
                        break;
                    }
                }
                if verbose {
                    use std::io::Write;
                    let outcome = if run_failed { "failed" } else { "succeeded" };
                    let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] {outcome}", started.elapsed().as_secs_f64());
                }
                totals.completed += 1;
                totals.failed += usize::from(run_failed);
                totals.elapsed = totals.elapsed.saturating_add(run_started.elapsed());
            }
            (totals, steps)
        });
    }
    let mut report = ScenarioReport {
        name: scenario.name.clone(),
        elapsed: Duration::ZERO,
        scenarios: Counts::default(),
        steps: scenario
            .steps
            .iter()
            .map(|step| (step.name.clone(), Counts::default()))
            .collect(),
    };
    while let Some(task) = tasks.join_next().await {
        let (totals, steps) = task?;
        report.scenarios.completed += totals.completed;
        report.scenarios.failed += totals.failed;
        report.scenarios.elapsed = report.scenarios.elapsed.saturating_add(totals.elapsed);
        for ((_, aggregate), step) in report.steps.iter_mut().zip(steps) {
            aggregate.completed += step.completed;
            aggregate.failed += step.failed;
            aggregate.elapsed = aggregate.elapsed.saturating_add(step.elapsed);
        }
    }
    report.elapsed = started.elapsed();
    Ok(report)
}
