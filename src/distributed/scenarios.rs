use super::{
    AgentLimits, Budget, Cancellation, DecodeLimits, DistributedHistogram, decode_histogram,
    encode_histogram, histogram::max_bytes_for_samples, transport::RunStatus,
};
use crate::scenario::{Counts, Scenario, ScenarioOptions, run_scenario_with_cancellation};
use serde::{Deserialize, Serialize};
use std::time::Duration;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub const MAX_SCENARIO_BYTES: usize = 1024 * 1024;
pub(super) const MAX_STEPS: usize = 8;
pub(super) const SCENARIO_LIMITS: AgentLimits = AgentLimits {
    max_samples: 10_000,
    max_concurrency: 8,
    max_rate_per_second: 1_000,
    max_duration: Duration::from_secs(60),
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioDescription {
    pub revision: [u8; 32],
    pub steps: Vec<String>,
}

impl ScenarioDescription {
    pub(super) fn valid(&self) -> bool {
        (1..=MAX_STEPS).contains(&self.steps.len())
            && self.steps.iter().enumerate().all(|(index, name)| {
                !name.trim().is_empty()
                    && name.len() <= 64
                    && !name.chars().any(char::is_control)
                    && !self.steps[..index].contains(name)
            })
    }
}

/// A locally installed scenario with fixed loopback destinations and agent ceilings.
#[derive(Clone)]
pub struct ScenarioProfile {
    pub(super) description: ScenarioDescription,
    scenario: Scenario,
    limits: AgentLimits,
    timeout: Duration,
    user_agent: Option<String>,
}

impl ScenarioProfile {
    pub fn from_json(
        input: &[u8],
        limits: AgentLimits,
        timeout: Duration,
        user_agent: Option<String>,
    ) -> Result<Self, BoxError> {
        if input.len() > MAX_SCENARIO_BYTES {
            return Err("distributed scenario exceeds 1 MiB".into());
        }
        if limits.max_samples > SCENARIO_LIMITS.max_samples
            || limits.max_concurrency > SCENARIO_LIMITS.max_concurrency
            || limits.max_rate_per_second > SCENARIO_LIMITS.max_rate_per_second
            || limits.max_duration > SCENARIO_LIMITS.max_duration
            || timeout > Duration::from_secs(10)
        {
            return Err("distributed scenario exceeds the local agent ceilings".into());
        }
        if let Some(value) = &user_agent {
            reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| "Grunt user_agent is not a valid HTTP header value")?;
        }
        let mut scenario = Scenario::from_json(input)?;
        scenario.restrict_to_loopback()?;
        let description = ScenarioDescription {
            revision: ring::digest::digest(&ring::digest::SHA256, input)
                .as_ref()
                .try_into()
                .expect("SHA-256 digest length"),
            steps: scenario.step_names().map(str::to_owned).collect(),
        };
        if !description.valid() {
            return Err("distributed scenarios require 1 to 8 steps with printable names of at most 64 bytes".into());
        }
        Ok(Self {
            description,
            scenario,
            limits,
            timeout,
            user_agent,
        })
    }

    pub(super) fn validate_budget(&self, budget: Budget) -> Result<(), BoxError> {
        if budget.requests.is_none() || budget.duration.is_some() {
            return Err("distributed scenarios require an iteration count".into());
        }
        super::preflight(budget, self.limits, self.timeout)?;
        Ok(())
    }

    pub(super) async fn run(
        self,
        budget: Budget,
        cancel: Cancellation,
    ) -> Result<ScenarioPayload, RunStatus> {
        let iterations = budget.requests.expect("validated scenario budget");
        let run = run_scenario_with_cancellation(
            self.scenario,
            iterations as usize,
            budget.concurrency as usize,
            self.timeout,
            ScenarioOptions {
                bounded_latency: true,
                rate: Some(budget.rate_per_second as u32),
                user_agent: self.user_agent,
                ..ScenarioOptions::default()
            },
            cancel.clone(),
        );
        tokio::pin!(run);
        let outcome = tokio::select! {
            biased;
            outcome = &mut run => outcome,
            () = tokio::time::sleep(self.limits.max_duration) => {
                cancel.cancel();
                let _ = run.await;
                return Err(RunStatus::AgentDeadline);
            }
        };
        let report = outcome
            .map_err(|_| RunStatus::Failed)?
            .ok_or(RunStatus::Cancelled)?;
        let counts: Vec<_> = std::iter::once(&report.scenarios)
            .chain(report.steps.iter().map(|(_, counts)| counts))
            .collect();
        let metadata = counts
            .iter()
            .map(|counts| Observation::from(*counts))
            .collect::<Vec<_>>();
        if !valid_counts(&metadata, self.description.steps.len(), iterations) {
            return Err(RunStatus::Failed);
        }
        let histograms = counts
            .iter()
            .zip(&metadata)
            .map(|(counts, observed)| {
                if let Some(growing) = counts.bounded_histogram() {
                    let mut fixed = crate::BoundedLatency::new();
                    fixed.merge(growing).map_err(|_| RunStatus::Failed)?;
                    encode_histogram(Some(&fixed), observed.completed)
                        .map_err(|_| RunStatus::Failed)
                } else {
                    encode_histogram(None, observed.completed).map_err(|_| RunStatus::Failed)
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(ScenarioPayload {
            metadata,
            histograms,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Observation {
    completed: u64,
    failed: u64,
}

impl From<&Counts> for Observation {
    fn from(counts: &Counts) -> Self {
        Self {
            completed: counts.completed as u64,
            failed: counts.failed as u64,
        }
    }
}

fn valid_counts(metadata: &[Observation], steps: usize, iterations: u64) -> bool {
    let Some((total, steps_metadata)) = metadata.split_first() else {
        return false;
    };
    if total.completed != iterations || total.failed > iterations || steps_metadata.len() != steps {
        return false;
    }
    let mut remaining = iterations;
    for step in steps_metadata {
        if step.completed != remaining || step.failed > remaining {
            return false;
        }
        remaining -= step.failed;
    }
    remaining == iterations - total.failed
}

pub(super) struct ScenarioPayload {
    pub metadata: Vec<Observation>,
    pub histograms: Vec<Vec<u8>>,
}

impl ScenarioPayload {
    pub fn bytes(&self) -> usize {
        self.histograms.iter().map(Vec::len).sum()
    }
}

pub(super) fn valid_lengths(lengths: &[usize], steps: usize, iterations: u64) -> bool {
    lengths.len() == steps + 1
        && lengths
            .iter()
            .all(|bytes| (1..=max_bytes_for_samples(iterations)).contains(bytes))
}

/// Merged counts and latency histogram for iterations or a single step.
#[derive(Clone, Default)]
pub struct ScenarioMeasurement {
    pub completed: u64,
    pub failed: u64,
    pub histogram: Option<DistributedHistogram>,
}

#[derive(Clone)]
pub(super) struct ScenarioAggregate {
    pub totals: ScenarioMeasurement,
    pub steps: Vec<ScenarioMeasurement>,
}

impl ScenarioAggregate {
    pub fn new(step_count: usize) -> Self {
        Self {
            totals: ScenarioMeasurement::default(),
            steps: vec![ScenarioMeasurement::default(); step_count],
        }
    }

    pub fn merge(&mut self, payload: ScenarioPayload, iterations: u64) -> Result<(), BoxError> {
        if !valid_counts(&payload.metadata, self.steps.len(), iterations)
            || payload.histograms.len() != payload.metadata.len()
        {
            return Err("inconsistent Grunt scenario results".into());
        }
        let mut merged = self.clone();
        let measures = std::iter::once(&mut merged.totals).chain(&mut merged.steps);
        for ((measure, observed), encoded) in measures.zip(payload.metadata).zip(payload.histograms)
        {
            let histogram = decode_histogram(
                &encoded,
                observed.completed,
                DecodeLimits {
                    max_envelope_bytes: max_bytes_for_samples(observed.completed),
                    max_samples: observed.completed,
                    assigned_requests: Some(observed.completed),
                },
            )?;
            if histogram.count() != observed.completed {
                return Err("Grunt scenario histogram does not match the completed count".into());
            }
            if let Some(current) = &mut measure.histogram {
                current.merge(&histogram)?;
            } else {
                measure.histogram = Some(histogram);
            }
            measure.completed = measure
                .completed
                .checked_add(observed.completed)
                .ok_or("scenario count overflow")?;
            measure.failed = measure
                .failed
                .checked_add(observed.failed)
                .ok_or("scenario count overflow")?;
        }
        *self = merged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scenario_profiles_accept_only_static_loopback_destinations() {
        for (url, allowed) in [
            ("http://127.0.0.1:8000/", true),
            ("http://[::1]:8000/", true),
            ("http://localhost/", false),
            ("http://192.0.2.5/", false),
            ("{{next}}", false),
        ] {
            let input = json!({
                "version": 1, "name": "local fixture",
                "steps": [
                    {"name": "start", "url": "http://127.0.0.1/",
                     "extract": {"next": {"from": "header", "name": "Location"}}},
                    {"name": "follow", "url": url}
                ]
            });
            assert_eq!(
                ScenarioProfile::from_json(
                    input.to_string().as_bytes(),
                    SCENARIO_LIMITS,
                    Duration::from_secs(1),
                    None,
                )
                .is_ok(),
                allowed,
                "{url}"
            );
        }
    }
}
