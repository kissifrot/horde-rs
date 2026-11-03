//! Local HTTP journeys with isolated virtual-user sessions.
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{
    Client, Method, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, REFERER},
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    error::Error,
    sync::{
        Arc, Mutex, OnceLock,
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
    variables: BTreeMap<String, VariableFile>,
    steps: Vec<StepFile>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VariableFile {
    Literal(String),
    Environment(EnvironmentVariable),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentVariable {
    env: String,
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
    form: Option<BTreeMap<String, String>>,
    json_body: Option<serde_json::Value>,
    #[serde(default)]
    wait_ms: u64,
    expect: Option<ExpectFile>,
    #[serde(default)]
    extract: BTreeMap<String, ExtractFile>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "from", rename_all = "lowercase", deny_unknown_fields)]
enum ExtractFile {
    Header { name: String },
    Json { pointer: String },
    Html { selector: String, attribute: String },
}

#[derive(Clone, Debug)]
enum Extract {
    Header(HeaderName),
    Json(String),
    Html { selector: String, attribute: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectFile {
    status: Option<Vec<u16>>,
    final_url: Option<String>,
    body_contains: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
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

#[derive(Clone, Debug)]
enum Part {
    Literal(String),
    Variable(String),
}

#[derive(Clone, Debug)]
struct Template {
    parts: Vec<Part>,
    dynamic: bool,
}

#[derive(Clone, Debug)]
enum JsonTemplate {
    Value(serde_json::Value),
    String(Template),
    Array(Vec<JsonTemplate>),
    Object(BTreeMap<String, JsonTemplate>),
}

impl JsonTemplate {
    fn compile(
        value: serde_json::Value,
        static_vars: &BTreeMap<String, String>,
        available: &std::collections::HashSet<String>,
    ) -> Result<Self, RunError> {
        Ok(match value {
            serde_json::Value::String(value) => {
                Self::String(Template::compile(&value, static_vars, available)?)
            }
            serde_json::Value::Array(items) => Self::Array(
                items
                    .into_iter()
                    .map(|value| Self::compile(value, static_vars, available))
                    .collect::<Result<_, _>>()?,
            ),
            serde_json::Value::Object(items) => Self::Object(
                items
                    .into_iter()
                    .map(|(key, value)| Ok((key, Self::compile(value, static_vars, available)?)))
                    .collect::<Result<_, RunError>>()?,
            ),
            value => Self::Value(value),
        })
    }

    fn dynamic(&self) -> bool {
        match self {
            Self::String(template) => template.dynamic,
            Self::Array(items) => items.iter().any(Self::dynamic),
            Self::Object(items) => items.values().any(Self::dynamic),
            Self::Value(_) => false,
        }
    }

    fn render(&self, variables: &BTreeMap<String, String>) -> Result<serde_json::Value, RunError> {
        Ok(match self {
            Self::Value(value) => value.clone(),
            Self::String(template) => serde_json::Value::String(template.render(variables)?),
            Self::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|value| value.render(variables))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object(items) => serde_json::Value::Object(
                items
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), value.render(variables)?)))
                    .collect::<Result<_, RunError>>()?,
            ),
        })
    }
}

fn render_form(
    fields: &[(String, Template)],
    variables: &BTreeMap<String, String>,
) -> Result<Bytes, RunError> {
    let fields: Vec<_> = fields
        .iter()
        .map(|(key, template)| Ok((key.as_str(), template.render(variables)?)))
        .collect::<Result<_, RunError>>()?;
    Ok(Bytes::from(serde_urlencoded::to_string(fields)?))
}

impl Template {
    fn compile(
        input: &str,
        static_vars: &BTreeMap<String, String>,
        available: &std::collections::HashSet<String>,
    ) -> Result<Self, RunError> {
        let mut parts = Vec::new();
        let mut rest = input;
        let mut dynamic = false;
        while let Some(start) = rest.find("{{") {
            parts.push(Part::Literal(rest[..start].to_owned()));
            let expression = &rest[start + 2..];
            let end = expression.find("}}").ok_or("unclosed scenario variable")?;
            let name = &expression[..end];
            if !valid_variable_name(name) {
                return Err("invalid scenario variable name".into());
            }
            if let Some(value) = static_vars.get(name) {
                parts.push(Part::Literal(value.clone()));
            } else if available.contains(name) {
                parts.push(Part::Variable(name.to_owned()));
                dynamic = true;
            } else {
                return Err(format!("undefined scenario variable: {name}").into());
            }
            rest = &expression[end + 2..];
        }
        parts.push(Part::Literal(rest.to_owned()));
        Ok(Self { parts, dynamic })
    }

    fn render(&self, variables: &BTreeMap<String, String>) -> Result<String, RunError> {
        let mut result = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(value) => result.push_str(value),
                Part::Variable(name) => result.push_str(
                    variables
                        .get(name)
                        .ok_or("scenario variable not available")?,
                ),
            }
        }
        Ok(result)
    }
}

use crate::redirect::referer as step_referer;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scenario_rejects_zero_duration_in_library_options() {
        let scenario = Scenario::from_json(br#"{"version":1,"name":"duration","steps":[{"name":"one","url":"http://127.0.0.1/"}]}"#).unwrap();
        let result = run_scenario_config(
            scenario,
            1,
            1,
            Duration::from_secs(1),
            ScenarioOptions {
                duration: Some(Duration::ZERO),
                ..Default::default()
            },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn scenario_rejects_out_of_range_rate_in_library_options() {
        for rate in [0, 1_000_000_001] {
            let scenario = Scenario::from_json(br#"{"version":1,"name":"rate","steps":[{"name":"one","url":"http://127.0.0.1/"}]}"#).unwrap();
            let result = run_scenario_config(
                scenario,
                1,
                1,
                Duration::from_secs(1),
                ScenarioOptions {
                    rate: Some(rate),
                    ..Default::default()
                },
            )
            .await;
            assert!(result.is_err());
        }
    }

    #[test]
    fn referer_policy_removes_credentials_fragments_and_cross_origin_paths() {
        let previous =
            Url::parse("https://user:pass@example.com/private?token=secret#fragment").unwrap();
        let same = Url::parse("https://example.com/next").unwrap();
        let other = Url::parse("https://auth.example.com/").unwrap();
        let downgrade = Url::parse("http://example.com/").unwrap();
        assert_eq!(
            step_referer(&previous, &same).unwrap(),
            "https://example.com/private?token=secret"
        );
        assert_eq!(
            step_referer(&previous, &other).unwrap(),
            "https://example.com/"
        );
        assert!(step_referer(&previous, &downgrade).is_none());
    }

    #[test]
    fn extracts_html_json_and_headers_without_logging_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-token", HeaderValue::from_static("header-value"));
        let mut sources = BTreeMap::new();
        sources.insert(
            "header".into(),
            Extract::Header(HeaderName::from_static("x-token")),
        );
        sources.insert("json".into(), Extract::Json("/item/token".into()));
        let values =
            extract_values(&sources, &headers, br#"{"item":{"token":"json-value"}}"#).unwrap();
        assert_eq!(values["header"], "header-value");
        assert_eq!(values["json"], "json-value");
        sources.clear();
        sources.insert(
            "csrf".into(),
            Extract::Html {
                selector: "input[name=csrf]".into(),
                attribute: "value".into(),
            },
        );
        let values = extract_values(
            &sources,
            &headers,
            br#"<input name="csrf" value="a&amp;b">"#,
        )
        .unwrap();
        assert_eq!(values["csrf"], "a&b");
        assert!(extract_values(&sources, &headers, b"<p>missing</p>").is_err());
    }

    #[test]
    fn dynamic_variables_require_a_preceding_unique_extractor() {
        for value in [
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"a","url":"http://example.com/{{later}}"},{"name":"b","url":"http://example.com/","extract":{"later":{"from":"header","name":"X-Value"}}}]}),
            serde_json::json!({"version":1,"name":"x","variables":{"token":"static"},"steps":[{"name":"a","url":"http://example.com/","extract":{"token":{"from":"header","name":"X-Value"}}}]}),
            serde_json::json!({"version":1,"name":"x","steps":[{"name":"a","url":"http://example.com/","extract":{"token":{"from":"html","selector":"[","attribute":"value"}}}]}),
        ] {
            assert!(Scenario::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn structured_json_sets_content_type_and_preserves_types() {
        let input = br#"{"version":1,"name":"json","variables":{"x":"quote\"slash\\"},"steps":[{"name":"post","url":"http://example.com/","json_body":{"nested":["{{x}}",true,42,null]}}]}"#;
        let scenario = Scenario::from_json(input).unwrap();
        let (_, headers, body) = scenario.steps[0].prepare(&BTreeMap::new()).unwrap();
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        let body: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"nested":["quote\"slash\\",true,42,null]})
        );
    }
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
    url: Option<Url>,
    dynamic_url: Option<Template>,
    method: Method,
    headers: HeaderMap,
    dynamic_headers: Vec<(HeaderName, Template)>,
    body: Option<Bytes>,
    dynamic_body: Option<Template>,
    form: Option<Vec<(String, Template)>>,
    json_body: Option<JsonTemplate>,
    wait: Duration,
    expect: Expect,
    extract: BTreeMap<String, Extract>,
}

#[derive(Clone, Debug, Default)]
struct Expect {
    status: Option<Vec<u16>>,
    final_url: Option<Url>,
    body_contains: Option<Bytes>,
    headers: HeaderMap,
}

fn validate_url(value: &str) -> Result<Url, RunError> {
    let url: Url = value.parse()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("step URLs must be absolute HTTP or HTTPS URLs".into());
    }
    Ok(url)
}

impl Step {
    fn prepare(
        &self,
        variables: &BTreeMap<String, String>,
    ) -> Result<(Url, HeaderMap, Option<Bytes>), RunError> {
        let url = match &self.dynamic_url {
            Some(template) => validate_url(&template.render(variables)?)?,
            None => self.url.clone().expect("validated static URL"),
        };
        let mut headers = self.headers.clone();
        for (name, template) in &self.dynamic_headers {
            headers.insert(
                name.clone(),
                HeaderValue::from_str(&template.render(variables)?)?,
            );
        }
        let body = match (&self.form, &self.json_body, &self.dynamic_body) {
            (Some(fields), _, _) => Some(render_form(fields, variables)?),
            (_, Some(json), _) => Some(Bytes::from(serde_json::to_vec(&json.render(variables)?)?)),
            (_, _, Some(template)) => Some(Bytes::from(template.render(variables)?)),
            _ => self.body.clone(),
        };
        Ok((url, headers, body))
    }
}

fn extract_values(
    sources: &BTreeMap<String, Extract>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<BTreeMap<String, String>, RunError> {
    let mut result = BTreeMap::new();
    let json = if sources
        .values()
        .any(|source| matches!(source, Extract::Json(_)))
    {
        Some(
            serde_json::from_slice::<serde_json::Value>(body)
                .map_err(|_| "response is not valid JSON")?,
        )
    } else {
        None
    };
    let html = if sources
        .values()
        .any(|source| matches!(source, Extract::Html { .. }))
    {
        Some(scraper::Html::parse_document(
            std::str::from_utf8(body).map_err(|_| "HTML is not UTF-8")?,
        ))
    } else {
        None
    };
    for (name, source) in sources {
        let value = match source {
            Extract::Header(header) => headers
                .get(header)
                .ok_or("response header missing")?
                .to_str()
                .map_err(|_| "response header is not text")?,
            Extract::Json(pointer) => json
                .as_ref()
                .and_then(|value| value.pointer(pointer))
                .and_then(|value| value.as_str())
                .ok_or("JSON pointer does not identify a string")?,
            Extract::Html {
                selector,
                attribute,
            } => {
                let selector =
                    scraper::Selector::parse(selector).map_err(|_| "invalid HTML selector")?;
                html.as_ref()
                    .and_then(|document| document.select(&selector).next())
                    .and_then(|element| element.value().attr(attribute))
                    .ok_or("HTML attribute missing")?
            }
        };
        result.insert(name.clone(), value.to_owned());
    }
    Ok(result)
}

impl Scenario {
    /// Parse and validate a JSON scenario, resolving declared environment variables before requests.
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
        let variables: BTreeMap<String, String> = file
            .variables
            .into_iter()
            .map(|(name, value)| {
                let value = match value {
                    VariableFile::Literal(value) => value,
                    VariableFile::Environment(source) => {
                        std::env::var(&source.env).map_err(|error| match error {
                            std::env::VarError::NotPresent => {
                                format!("environment variable {} is not set", source.env)
                            }
                            std::env::VarError::NotUnicode(_) => {
                                format!("environment variable {} is not valid UTF-8", source.env)
                            }
                        })?
                    }
                };
                Ok((name, value))
            })
            .collect::<Result<_, String>>()?;
        let mut names = std::collections::HashSet::new();
        let mut available = std::collections::HashSet::new();
        let mut steps = Vec::with_capacity(file.steps.len());
        for step in file.steps {
            if step.name.trim().is_empty() || !names.insert(step.name.clone()) {
                return Err("step names must be nonempty and unique".into());
            }
            let url_template = Template::compile(&step.url, &variables, &available)?;
            let (url, dynamic_url) = if url_template.dynamic {
                (None, Some(url_template))
            } else {
                (
                    Some(validate_url(&url_template.render(&BTreeMap::new())?)?),
                    None,
                )
            };
            let mut headers = HeaderMap::new();
            let mut dynamic_headers = Vec::new();
            for (key, value) in step.headers {
                let key = HeaderName::from_bytes(key.as_bytes())?;
                let value = Template::compile(&value, &variables, &available)?;
                if value.dynamic {
                    dynamic_headers.push((key, value));
                } else {
                    headers.insert(
                        key,
                        HeaderValue::from_str(&value.render(&BTreeMap::new())?)?,
                    );
                }
            }
            let (mut body, dynamic_body) = match step.body {
                Some(value) => {
                    let template = Template::compile(&value, &variables, &available)?;
                    if template.dynamic {
                        (None, Some(template))
                    } else {
                        (Some(Bytes::from(template.render(&BTreeMap::new())?)), None)
                    }
                }
                None => (None, None),
            };
            let mut form = None;
            let mut json_body = None;
            let mut default_content_type = None;
            if let Some(fields) = step.form {
                let fields: Vec<_> = fields
                    .into_iter()
                    .map(|(name, value)| {
                        Ok((name, Template::compile(&value, &variables, &available)?))
                    })
                    .collect::<Result<_, RunError>>()?;
                default_content_type = Some("application/x-www-form-urlencoded");
                if fields.iter().any(|(_, value)| value.dynamic) {
                    form = Some(fields);
                } else {
                    body = Some(render_form(&fields, &BTreeMap::new())?);
                }
            }
            if let Some(value) = step.json_body {
                let template = JsonTemplate::compile(value, &variables, &available)?;
                default_content_type = Some("application/json");
                if template.dynamic() {
                    json_body = Some(template);
                } else {
                    body = Some(Bytes::from(serde_json::to_vec(
                        &template.render(&BTreeMap::new())?,
                    )?));
                }
            }
            if let Some(value) = default_content_type
                && !headers.contains_key(CONTENT_TYPE)
                && !dynamic_headers.iter().any(|(name, _)| name == CONTENT_TYPE)
            {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            }
            let expect = if let Some(expect) = step.expect {
                let final_url = expect
                    .final_url
                    .map(|url| -> Result<Url, RunError> {
                        let url: Url = expand(&url, &variables)?.parse()?;
                        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                            return Err("expected final URL must use HTTP or HTTPS".into());
                        }
                        Ok(url)
                    })
                    .transpose()?;
                let body_contains = expect
                    .body_contains
                    .map(|body| expand(&body, &variables))
                    .transpose()?;
                if body_contains.as_ref().is_some_and(String::is_empty) {
                    return Err(
                        "expected body text must not be empty after variable substitution".into(),
                    );
                }
                let mut expected_headers = HeaderMap::new();
                for (name, value) in expect.headers {
                    expected_headers.insert(
                        HeaderName::from_bytes(name.as_bytes())?,
                        HeaderValue::from_str(&expand(&value, &variables)?)?,
                    );
                }
                Expect {
                    status: expect.status,
                    final_url,
                    body_contains: body_contains.map(Bytes::from),
                    headers: expected_headers,
                }
            } else {
                Expect::default()
            };
            let mut extract = BTreeMap::new();
            for (name, source) in step.extract {
                if variables.contains_key(&name) || !available.insert(name.clone()) {
                    return Err(format!("duplicate scenario variable: {name}").into());
                }
                let source = match source {
                    ExtractFile::Header { name } => {
                        Extract::Header(HeaderName::from_bytes(name.as_bytes())?)
                    }
                    ExtractFile::Json { pointer } => {
                        if !pointer.is_empty()
                            && (!pointer.starts_with('/')
                                || pointer.split('~').skip(1).any(|escape| {
                                    !escape.starts_with('0') && !escape.starts_with('1')
                                }))
                        {
                            return Err("invalid JSON pointer".into());
                        }
                        Extract::Json(pointer)
                    }
                    ExtractFile::Html {
                        selector,
                        attribute,
                    } => {
                        scraper::Selector::parse(&selector).map_err(|_| "invalid HTML selector")?;
                        Extract::Html {
                            selector,
                            attribute,
                        }
                    }
                };
                extract.insert(name, source);
            }
            steps.push(Step {
                name: step.name,
                url,
                dynamic_url,
                method: step.method.parse()?,
                headers,
                dynamic_headers,
                body,
                dynamic_body,
                form,
                json_body,
                wait: Duration::from_millis(step.wait_ms),
                expect,
                extract,
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
    /// Final HTTP status counts; requests that receive no final response have none.
    pub status_codes: BTreeMap<u16, usize>,
    /// Failure categories may overlap within one step.
    pub failure_details: BTreeMap<&'static str, usize>,
    latencies: Vec<Duration>,
    bounded_latencies: Option<crate::BoundedLatency>,
    latency_overflow: bool,
}

impl Counts {
    fn record(
        &mut self,
        latency: Duration,
        failed: bool,
        status: Option<u16>,
        errors: &[(bool, &'static str)],
        bounded: bool,
    ) {
        self.completed += 1;
        self.failed += usize::from(failed);
        self.elapsed = self.elapsed.saturating_add(latency);
        if let Some(code) = status {
            *self.status_codes.entry(code).or_default() += 1;
        }
        for (_, error) in errors.iter().filter(|(active, _)| *active) {
            *self.failure_details.entry(error).or_default() += 1;
        }
        if bounded {
            if self
                .bounded_latencies
                .get_or_insert_with(crate::BoundedLatency::new)
                .record(latency)
                .is_err()
            {
                self.latency_overflow = true;
            }
        } else {
            self.latencies.push(latency);
        }
    }

    fn merge(&mut self, other: Self) -> Result<(), RunError> {
        if other.latency_overflow {
            return Err("latency exceeds bounded histogram range or capacity".into());
        }
        self.completed += other.completed;
        self.failed += other.failed;
        self.elapsed = self.elapsed.saturating_add(other.elapsed);
        for (code, count) in other.status_codes {
            *self.status_codes.entry(code).or_default() += count;
        }
        for (name, count) in other.failure_details {
            *self.failure_details.entry(name).or_default() += count;
        }
        if self.latencies.is_empty() {
            self.latencies = other.latencies;
        } else {
            self.latencies.extend(other.latencies);
        }
        if let Some(histogram) = other.bounded_latencies {
            if let Some(current) = &mut self.bounded_latencies {
                current.merge(&histogram)?;
            } else {
                self.bounded_latencies = Some(histogram);
            }
        }
        Ok(())
    }

    /// Consume collected latency samples without copying; returns None for skipped steps.
    pub fn take_latency_stats(&mut self) -> Option<crate::LatencyStats> {
        match self.bounded_latencies.take() {
            Some(histogram) => Some(crate::LatencyStats::from_bounded(histogram)),
            None => crate::LatencyStats::from_samples(std::mem::take(&mut self.latencies)),
        }
    }
}

/// Scenario execution options.
#[derive(Clone, Debug, Default)]
pub struct ScenarioOptions {
    pub verbose: bool,
    pub max_redirects: Option<usize>,
    pub auto_referer: bool,
    pub bounded_latency: bool,
    /// Default User-Agent, overridden by an explicit step header.
    pub user_agent: Option<String>,
    /// Stop starting new iterations after this duration; active iterations finish.
    pub duration: Option<Duration>,
    /// Maximum target rate of iteration starts per second across all users.
    pub rate: Option<u32>,
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
    run_scenario_with_referer(
        scenario,
        iterations,
        users,
        timeout,
        verbose,
        max_redirects,
        false,
    )
    .await
}

/// Run a scenario with optional redirects and automatic step-to-step referers.
pub async fn run_scenario_with_referer(
    scenario: Scenario,
    iterations: usize,
    users: usize,
    timeout: Duration,
    verbose: bool,
    max_redirects: Option<usize>,
    auto_referer: bool,
) -> Result<ScenarioReport, RunError> {
    run_scenario_config(
        scenario,
        iterations,
        users,
        timeout,
        ScenarioOptions {
            verbose,
            max_redirects,
            auto_referer,
            bounded_latency: false,
            user_agent: None,
            duration: None,
            rate: None,
        },
    )
    .await
}

/// Run a scenario using the given execution options.
pub async fn run_scenario_config(
    scenario: Scenario,
    iterations: usize,
    users: usize,
    timeout: Duration,
    options: ScenarioOptions,
) -> Result<ScenarioReport, RunError> {
    let ScenarioOptions {
        verbose,
        max_redirects,
        auto_referer,
        bounded_latency,
        user_agent,
        duration,
        rate,
    } = options;
    if iterations == 0 || users == 0 || timeout.is_zero() {
        return Err("iterations, users and timeout must be greater than zero".into());
    }
    if max_redirects == Some(0) {
        return Err("max redirects must be greater than zero".into());
    }
    if duration.is_some_and(|duration| duration.is_zero()) {
        return Err("duration must be greater than zero".into());
    }
    if rate.is_some_and(|rate| !(1..=1_000_000_000).contains(&rate)) {
        return Err("rate must be between 1 and 1000000000 iterations per second".into());
    }
    let user_agent = reqwest::header::HeaderValue::from_str(
        user_agent
            .as_deref()
            .unwrap_or(concat!("Horde/", env!("CARGO_PKG_VERSION"))),
    )?;
    let scenario = Arc::new(scenario);
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    let started = Instant::now();
    let next_start = rate.map(|_| Arc::new(Mutex::new(started)));
    for user_id in 1..=users.min(iterations) {
        let scenario = Arc::clone(&scenario);
        let next = Arc::clone(&next);
        let next_start = next_start.clone();
        let client = Client::builder()
            .cookie_store(true)
            .timeout(timeout)
            .user_agent(user_agent.clone())
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        tasks.spawn(async move {
            let mut totals = Counts::default();
            let mut steps = vec![Counts::default(); scenario.steps.len()];
            let mut previous_url: Option<Url> = None;
            while let Ok(index) = next.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < iterations).then(|| n + 1)
                }) {
                let iteration = index + 1;
                if let (Some(rate), Some(next_start)) = (rate, &next_start) {
                    let scheduled = {
                        let mut next_time = next_start.lock().unwrap();
                        let scheduled = (*next_time).max(Instant::now());
                        *next_time = scheduled + Duration::from_secs_f64(1.0 / f64::from(rate));
                        scheduled
                    };
                    if duration.is_some_and(|duration| scheduled.duration_since(started) >= duration) {
                        break;
                    }
                    tokio::time::sleep_until(scheduled.into()).await;
                }
                let run_started = Instant::now();
                if duration.is_some_and(|duration| run_started.duration_since(started) >= duration) {
                    break;
                }
                if verbose {
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] started", started.elapsed().as_secs_f64());
                }
                let mut run_failed = false;
                // Extracted values are isolated per user and reset for each iteration.
                let mut variables = BTreeMap::new();
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
                    let (target_url, headers, request_body) = match step.prepare(&variables) {
                        Ok(prepared) => prepared,
                        Err(_) => {
                            if verbose {
                                use std::io::Write;
                                let _ = writeln!(std::io::stderr().lock(), "[+{:.3}s] [user {user_id} scenario {iteration}] step {:?} failed errors=variable", started.elapsed().as_secs_f64(), step.name);
                            }
                            steps[index].record(step_started.elapsed(), true, None, &[(true, "variable")], bounded_latency);
                            run_failed = true;
                            break;
                        }
                    };
                    let referer = if auto_referer && !headers.contains_key(REFERER) {
                        previous_url.as_ref().and_then(|previous| step_referer(previous, &target_url))
                    } else { None };
                    let mut request = client.request(step.method.clone(), target_url).headers(headers);
                    if let Some(referer) = referer {
                        request = request.header(REFERER, referer);
                    }
                    if let Some(body) = request_body {
                        request = request.body(body);
                    }
                    let mut status = None;
                    let mut bytes = 0u64;
                    let mut http_error = false;
                    let mut timeout_error = false;
                    let mut connection_error = false;
                    let mut body_error = false;
                    let mut redirect_error = false;
                    let mut url_assertion_error = false;
                    let mut header_assertion_error = false;
                    let mut body_assertion_error = false;
                    let mut extraction_error = false;
                    let mut other_error = false;
                    let result = if let Some(limit) = max_redirects {
                        match request.build() {
                            Ok(request) => crate::redirect::send(&client, request, limit, timeout, None).await,
                            Err(error) => Err(crate::redirect::RedirectError::Request(error)),
                        }
                    } else {
                        request.send().await.map_err(crate::redirect::RedirectError::Request)
                    };
                    let failed = match result {
                        Ok(response) => {
                            if auto_referer {
                                previous_url = Some(response.url().clone());
                            }
                            status = Some(response.status().as_u16());
                            let code = response.status().as_u16();
                            let status_matches = step.expect.status.as_ref().map_or_else(|| response.status().is_success(), |allowed| allowed.contains(&code));
                            http_error = !status_matches;
                            url_assertion_error = step.expect.final_url.as_ref().is_some_and(|expected| expected != response.url());
                            header_assertion_error = step.expect.headers.iter().any(|(name, value)| !response.headers().get_all(name).iter().any(|actual| actual == value));
                            let headers = if step.extract.is_empty() { HeaderMap::new() } else { response.headers().clone() };
                            let capture_body = step.extract.values().any(|source| matches!(source, Extract::Json(_) | Extract::Html { .. }));
                            let mut captured = Vec::new();
                            let mut found = step.expect.body_contains.is_none();
                            let mut tail = Vec::new();
                            let mut body = response.bytes_stream();
                            while let Some(chunk) = body.next().await {
                                match chunk {
                                    Ok(chunk) => {
                                        if verbose { bytes = bytes.saturating_add(chunk.len() as u64); }
                                        if capture_body {
                                            const MAX_EXTRACT_BODY: usize = 2 * 1024 * 1024;
                                            if chunk.len() > MAX_EXTRACT_BODY.saturating_sub(captured.len()) {
                                                extraction_error = true;
                                                break;
                                            }
                                            captured.extend_from_slice(&chunk);
                                        }
                                        if let Some(needle) = step.expect.body_contains.as_ref().filter(|_| !found) {
                                            let keep = needle.len() - 1;
                                            let prefix = keep.min(chunk.len());
                                            tail.extend_from_slice(&chunk[..prefix]);
                                            found = tail.windows(needle.len()).any(|window| window == needle.as_ref())
                                                || chunk.windows(needle.len()).any(|window| window == needle.as_ref());
                                            if chunk.len() >= keep {
                                                tail.clear();
                                                tail.extend_from_slice(&chunk[chunk.len() - keep..]);
                                            } else if tail.len() > keep {
                                                tail.drain(..tail.len() - keep);
                                            }
                                        }
                                    }
                                    Err(error) => { body_error = true; timeout_error = error.is_timeout(); break; }
                                }
                            }
                            body_assertion_error = !found;
                            if !step.extract.is_empty() && !http_error && !body_error && !url_assertion_error && !header_assertion_error && !body_assertion_error && !extraction_error {
                                match extract_values(&step.extract, &headers, &captured) {
                                    Ok(extracted) => variables.extend(extracted),
                                    Err(_) => extraction_error = true,
                                }
                            }
                            http_error || body_error || url_assertion_error || header_assertion_error || body_assertion_error || extraction_error
                        }
                        Err(error) => {
                            timeout_error = error.is_timeout();
                            connection_error = error.is_connect();
                            redirect_error = matches!(error, crate::redirect::RedirectError::Limit | crate::redirect::RedirectError::InvalidScheme);
                            other_error = !timeout_error && !connection_error && !redirect_error;
                            true
                        },
                    };
                    let latency = step_started.elapsed();
                    let errors = [(http_error, "status"), (url_assertion_error, "final_url"), (header_assertion_error, "header_assertion"), (body_assertion_error, "body_assertion"), (extraction_error, "extraction"), (timeout_error, "timeout"), (connection_error, "connection"), (body_error, "body"), (redirect_error, "redirect"), (other_error, "other")];
                    if verbose {
                        use std::io::Write;
                        let mut stderr = std::io::stderr().lock();
                        let status = status.map_or_else(|| "none".to_owned(), |code| code.to_string());
                        let _ = write!(stderr, "[+{:.3}s] [user {user_id} scenario {iteration}] step {:?} completed status={status} bytes={bytes} latency_ms={:.2}", started.elapsed().as_secs_f64(), step.name, latency.as_secs_f64() * 1000.0);
                        if failed {
                            let _ = write!(stderr, " errors=");
                            let mut separator = "";
                            for (_, label) in errors.into_iter().filter(|(active, _)| *active) {
                                let _ = write!(stderr, "{separator}{label}");
                                separator = ",";
                            }
                        }
                        let _ = writeln!(stderr);
                    }
                    steps[index].record(latency, failed, status, &errors, bounded_latency);
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
                totals.record(run_started.elapsed(), run_failed, None, &[], bounded_latency);
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
        report.scenarios.merge(totals)?;
        for ((_, aggregate), step) in report.steps.iter_mut().zip(steps) {
            aggregate.merge(step)?;
        }
    }
    report.elapsed = started.elapsed();
    Ok(report)
}
