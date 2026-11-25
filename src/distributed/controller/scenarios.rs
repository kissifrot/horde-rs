use super::{
    BoxError, Controller, Grunt, GruntConnection, IO_TIMEOUT, MAX_RESULT_METADATA, Operation,
    Outcome, PREFLIGHT_CONCURRENCY, Reply, RunStatus, exchange, plan_budgets, read_frame_limited,
};
pub use crate::distributed::scenarios::ScenarioMeasurement;
use crate::distributed::{
    AgentStatus, Budget, Cancellation,
    scenarios::{Observation, SCENARIO_LIMITS, ScenarioAggregate, ScenarioPayload, valid_lengths},
    transport::{ErrorCode, ScenarioDescription, valid_profile_name},
};
use futures_util::{StreamExt, future::join_all, stream};
use std::{net::SocketAddr, time::Duration};
use tokio::net::TcpStream;
use tokio::{
    io::AsyncReadExt,
    time::{Instant, timeout, timeout_at},
};
use tokio_rustls::client::TlsStream;

pub struct ScenarioControllerReport {
    pub totals: ScenarioMeasurement,
    pub steps: Vec<(String, ScenarioMeasurement)>,
    pub agents: Vec<ScenarioAgent>,
    pub elapsed: Duration,
    pub interrupted: bool,
}

pub struct ScenarioAgent {
    pub address: SocketAddr,
    pub status: Option<AgentStatus>,
    pub rejection: Option<ErrorCode>,
}

pub struct ScenarioPreflight {
    pub epoch: [u8; 16],
    pub description: ScenarioDescription,
}

impl ScenarioControllerReport {
    pub fn is_partial(&self) -> bool {
        self.interrupted
            || self
                .agents
                .iter()
                .any(|agent| agent.status != Some(AgentStatus::Complete))
    }
}

impl Controller {
    /// Read the installed scenario from every peer in inventory order, without a workload budget.
    pub async fn preflight_scenario(
        &self,
        grunts: &[Grunt],
        profile: &str,
        cancel: Cancellation,
    ) -> Result<Vec<Result<ScenarioPreflight, BoxError>>, BoxError> {
        if !valid_profile_name(profile) {
            return Err("invalid scenario profile name".into());
        }
        self.validate_grunts(grunts)?;
        let mut run_id = [0; 16];
        getrandom::fill(&mut run_id).map_err(|_| "could not generate scenario check identifier")?;
        Ok(stream::iter(0..grunts.len())
            .map(|index| {
                let cancel = &cancel;
                async move {
                    let ((epoch, _), description) = self
                        .describe(&grunts[index], run_id, profile, None, cancel)
                        .await?;
                    Ok(ScenarioPreflight { epoch, description })
                }
            })
            .buffered(PREFLIGHT_CONCURRENCY)
            .collect()
            .await)
    }

    /// Check the installed revision on every assigned peer before submitting any load.
    pub async fn run_scenario(
        &self,
        grunts: &[Grunt],
        profile: &str,
        budget: Budget,
        deadline: Duration,
        cancel: Cancellation,
    ) -> Result<ScenarioControllerReport, BoxError> {
        if !valid_profile_name(profile) || budget.requests.is_none() || budget.duration.is_some() {
            return Err("invalid scenario profile or iteration budget".into());
        }
        self.validate_grunts(grunts)?;
        let shares = plan_budgets(budget, grunts.len(), deadline)?;
        for share in &shares {
            SCENARIO_LIMITS.validate(*share)?;
        }
        let mut run_id = [0; 16];
        getrandom::fill(&mut run_id).map_err(|_| "could not generate run identifier")?;
        let start = Instant::now();
        let by = start + deadline;
        let active = &grunts[..shares.len()];
        let mut prepared = Vec::with_capacity(active.len());
        let mut description: Option<ScenarioDescription> = None;
        let results = join_all(active.iter().zip(&shares).map(|(grunt, share)| {
            timeout_at(
                by,
                self.describe(grunt, run_id, profile, Some(*share), &cancel),
            )
        }))
        .await;
        for (grunt, result) in active.iter().zip(results) {
            let (connection, found) = result
                .map_err(|_| {
                    format!(
                        "Grunt {}: scenario preflight deadline exceeded",
                        grunt.address
                    )
                })?
                .map_err(|error| format!("Grunt {}: {error}", grunt.address))?;
            if description
                .as_ref()
                .is_some_and(|expected| expected != &found)
            {
                return Err(format!(
                    "Grunt {}: scenario {profile:?} has a different revision or step list",
                    grunt.address
                )
                .into());
            }
            description.get_or_insert(found);
            prepared.push(connection);
        }
        let description = description.expect("nonempty budget shares");
        let mut aggregate = ScenarioAggregate::new(description.steps.len());
        let outcomes = join_all(active.iter().zip(&shares).zip(prepared).map(
            |((grunt, share), connection)| {
                let cancel = cancel.clone();
                let submission = Operation::SubmitScenario {
                    profile: profile.to_owned(),
                    revision: description.revision,
                    budget: *share,
                };
                let expected = Some((
                    description.steps.len(),
                    share.requests.expect("iteration share"),
                ));
                async move {
                    timeout_at(
                        by,
                        Self::drive_connected(connection, run_id, submission, expected, cancel),
                    )
                    .await
                    .ok()
                    .and_then(|result| {
                        result
                            .inspect_err(|error| eprintln!("Grunt {}: {error}", grunt.address))
                            .ok()
                    })
                }
            },
        ))
        .await;
        let agents = active
            .iter()
            .zip(shares)
            .zip(outcomes)
            .map(|((grunt, share), outcome)| {
                let mut rejection = None;
                let status = match outcome {
                    Some(Outcome::Scenario(payload)) => {
                        match aggregate.merge(payload, share.requests.expect("iteration share")) {
                            Ok(()) => Some(AgentStatus::Complete),
                            Err(_) => Some(AgentStatus::Invalid),
                        }
                    }
                    Some(Outcome::State(status)) => Some(match status {
                        RunStatus::Cancelled => AgentStatus::Cancelled,
                        RunStatus::AgentDeadline => AgentStatus::AgentDeadline,
                        RunStatus::SampleLimit => AgentStatus::SampleLimit,
                        _ => AgentStatus::Failed,
                    }),
                    Some(Outcome::Rejected(code)) => {
                        rejection = Some(code);
                        Some(AgentStatus::Failed)
                    }
                    Some(Outcome::Result(..)) => Some(AgentStatus::Invalid),
                    None => None,
                };
                ScenarioAgent {
                    address: grunt.address,
                    status,
                    rejection,
                }
            })
            .collect();
        Ok(ScenarioControllerReport {
            totals: aggregate.totals,
            steps: description.steps.into_iter().zip(aggregate.steps).collect(),
            agents,
            elapsed: start.elapsed(),
            interrupted: cancel.is_cancelled(),
        })
    }

    async fn describe(
        &self,
        grunt: &Grunt,
        run_id: [u8; 16],
        profile: &str,
        budget: Option<Budget>,
        cancel: &Cancellation,
    ) -> Result<(GruntConnection, ScenarioDescription), BoxError> {
        let mut signal = cancel.subscribe();
        tokio::select! {
            biased;
            _ = crate::wait_for_cancel(&mut signal) => Err("scenario preflight interrupted".into()),
            result = async {
                let (epoch, mut stream) = timeout(IO_TIMEOUT, self.connect(grunt)).await??;
                let reply = exchange(&mut stream, epoch, run_id, Operation::DescribeScenario {
                    profile: profile.to_owned(),
                    budget,
                }).await?;
                match reply {
                    Reply::ScenarioProfile { description } if description.valid() => Ok(((epoch, stream), description)),
                    Reply::Error { code: ErrorCode::UnknownProfile } => Err(format!("scenario {profile:?} is not installed").into()),
                    Reply::Error { code: ErrorCode::Rejected } => Err("scenario iteration budget exceeds local limits".into()),
                    _ => Err("invalid scenario profile description".into()),
                }
            } => result,
        }
    }
}

pub(super) async fn collect(
    stream: &mut TlsStream<TcpStream>,
    reply: Reply,
    steps: usize,
    iterations: u64,
) -> Result<Outcome, BoxError> {
    let Reply::ScenarioResult { histogram_bytes } = reply else {
        return Err("missing completed Grunt scenario result".into());
    };
    if !valid_lengths(&histogram_bytes, steps, iterations) {
        return Err("invalid Grunt scenario result lengths".into());
    }
    let metadata = read_frame_limited::<Vec<Observation>>(stream, MAX_RESULT_METADATA)
        .await?
        .ok_or("missing Grunt scenario result metadata")?;
    let mut histograms = Vec::with_capacity(histogram_bytes.len());
    for bytes in histogram_bytes {
        let mut histogram = vec![0; bytes];
        stream.read_exact(&mut histogram).await?;
        histograms.push(histogram);
    }
    Ok(Outcome::Scenario(ScenarioPayload {
        metadata,
        histograms,
    }))
}
