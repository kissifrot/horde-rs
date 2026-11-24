//! Building blocks for distributed runs with experimental private control transport.
#![forbid(unsafe_code)]

#[cfg(feature = "distributed-transport")]
mod control_network;
mod histogram;
mod policy;
mod result;
#[cfg(feature = "distributed-transport")]
pub mod transport;
pub use histogram::{
    DecodeLimits, DistributedHistogram, MAX_ENVELOPE_BYTES, decode_histogram, encode_histogram,
};
pub use policy::{GruntPolicy, ResourceLimits};
pub use result::{CompletedResult, CompletionReason, ResultMetrics};

/// Outcome of a single local agent run; partial runs never expose metrics.
#[derive(Debug)]
pub enum LocalOutcome {
    Complete {
        report: Box<crate::Report>,
        reason: StopReason,
        started: u64,
    },
    Partial {
        reason: StopReason,
    },
}

pub(crate) struct LocalControl {
    pub budget: Budget,
    pub limits: AgentLimits,
    pub cancel: tokio::sync::watch::Receiver<bool>,
    pub policy: GruntPolicy,
}

pub use crate::Cancellation;

/// Local deadline information to disclose before running an assigned budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunPreflight {
    pub agent_deadline: std::time::Duration,
    /// `None` means the request-count-only run has no fixed start window.
    pub drain_may_hit_deadline: Option<bool>,
}

pub fn preflight(
    budget: Budget,
    limits: AgentLimits,
    timeout: std::time::Duration,
) -> Result<RunPreflight, Error> {
    limits.validate(budget)?;
    if timeout.is_zero() {
        return Err(Error("invalid request timeout"));
    }
    std::time::Instant::now()
        .checked_add(timeout)
        .ok_or(Error("request timeout out of range"))?;
    let drain_may_hit_deadline = budget
        .duration
        .map(|duration| {
            duration
                .checked_add(timeout)
                .map(|end| end >= limits.max_duration)
                .ok_or(Error("timeout duration out of range"))
        })
        .transpose()?;
    Ok(RunPreflight {
        agent_deadline: limits.max_duration,
        drain_may_hit_deadline,
    })
}

/// Execute one agent locally. This API opens no listener or distributed transport.
/// Call `Cancellation::cancel` to request a run-level cancellation.
pub async fn run_local(
    plan: crate::TestPlan,
    budget: Budget,
    limits: AgentLimits,
    policy: &GruntPolicy,
    cancel: Cancellation,
) -> Result<LocalOutcome, Box<dyn std::error::Error + Send + Sync>> {
    preflight(budget, limits, plan.timeout)?;
    let _reservation = policy.reserve(&plan, budget, limits)?;
    run_reserved(plan, budget, limits, policy, cancel).await
}

async fn run_reserved(
    mut plan: crate::TestPlan,
    budget: Budget,
    limits: AgentLimits,
    policy: &GruntPolicy,
    cancel: Cancellation,
) -> Result<LocalOutcome, Box<dyn std::error::Error + Send + Sync>> {
    plan.requests = match budget.requests {
        Some(n) => usize::try_from(n)?,
        None => usize::MAX,
    };
    plan.concurrency = usize::try_from(budget.concurrency)?;
    plan.duration = budget.duration;
    plan.rate = Some(u32::try_from(budget.rate_per_second)?);
    plan.bounded_latency = true;
    let receiver = cancel.subscribe();
    match crate::run_impl(
        plan,
        Some(LocalControl {
            budget,
            limits,
            cancel: receiver,
            policy: policy.clone(),
        }),
        None,
        None,
    )
    .await?
    {
        crate::RunOutcome::OfflineComplete {
            report,
            reason,
            started,
        } => Ok(LocalOutcome::Complete {
            report: Box::new(report),
            reason,
            started,
        }),
        crate::RunOutcome::OfflinePartial(reason) => Ok(LocalOutcome::Partial { reason }),
        crate::RunOutcome::Standalone(_) => Err("unexpected standalone outcome".into()),
        crate::RunOutcome::Cancelled => Err("unexpected local cancellation".into()),
    }
}

/// Errors deliberately contain no peer-supplied data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(&'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}

#[cfg(feature = "distributed-transport")]
pub mod controller;

/// A global budget, split only between the agents that can receive nonzero shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub requests: Option<u64>,
    pub concurrency: u64,
    pub rate_per_second: u64,
    pub duration: Option<std::time::Duration>,
}

/// Trusted agent ceilings. An omitted request budget still needs `max_samples`.
#[derive(Clone, Copy, Debug)]
pub struct AgentLimits {
    pub max_samples: u64,
    pub max_concurrency: u64,
    pub max_rate_per_second: u64,
    pub max_duration: std::time::Duration,
}

impl AgentLimits {
    /// Reject an infeasible share; no budget is silently reduced or reassigned.
    pub fn validate(&self, share: Budget) -> Result<(), Error> {
        if self.max_samples == 0
            || self.max_concurrency == 0
            || self.max_rate_per_second == 0
            || self.max_duration.is_zero()
            || share.concurrency == 0
            || share.rate_per_second == 0
            || share.requests == Some(0)
            || share.duration == Some(std::time::Duration::ZERO)
            || (share.requests.is_none() && share.duration.is_none())
        {
            return Err(Error("invalid agent limits or budget"));
        }
        // Duration-only runs reserve usize::MAX as the scheduler's unbounded sentinel.
        if share.concurrency > self.max_concurrency
            || share.rate_per_second > self.max_rate_per_second
            || share.rate_per_second > 1_000_000_000
            || usize::try_from(share.concurrency).is_err()
            || share.requests.is_some_and(|v| usize::try_from(v).is_err())
            || (share.requests.is_none()
                && usize::try_from(self.max_samples)
                    .ok()
                    .is_none_or(|n| n == usize::MAX))
            || share.requests.is_some_and(|v| v > self.max_samples)
            || share.duration.is_some_and(|v| v >= self.max_duration)
        {
            return Err(Error("agent capacity exceeded"));
        }
        Ok(())
    }
}

/// Reason why no more requests may start. Local limits make a run partial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    RequestBudget,
    Duration,
    AgentDeadline,
    SampleLimit,
    Cancelled,
}

impl StopReason {
    pub fn is_partial(self) -> bool {
        matches!(
            self,
            Self::AgentDeadline | Self::SampleLimit | Self::Cancelled
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartDecision {
    Start,
    AtConcurrency,
    Stopped(StopReason),
}

/// Offline start gate. A future executor must use it for every start and completion.
pub struct StartGate {
    budget: Budget,
    limits: AgentLimits,
    started_at: std::time::Instant,
    last_checked: std::time::Instant,
    started: u64,
    in_flight: u64,
    stopped: Option<StopReason>,
    terminal: Option<StopReason>,
}

impl StartGate {
    pub fn new(
        budget: Budget,
        limits: AgentLimits,
        now: std::time::Instant,
    ) -> Result<Self, Error> {
        limits.validate(budget)?;
        now.checked_add(limits.max_duration)
            .ok_or(Error("deadline out of range"))?;
        Ok(Self {
            budget,
            limits,
            started_at: now,
            last_checked: now,
            started: 0,
            in_flight: 0,
            stopped: None,
            terminal: None,
        })
    }

    /// Checks count, duration and local limits before granting a start.
    /// Rate scheduling is separate and must occur before calling this method.
    pub fn try_start(&mut self, now: std::time::Instant) -> Result<StartDecision, Error> {
        self.observe(now)?;
        if let Some(reason) = self.terminal {
            return Ok(StartDecision::Stopped(reason));
        }
        if let Some(reason) = self.stopped {
            return Ok(StartDecision::Stopped(reason));
        }
        if self.in_flight >= self.budget.concurrency {
            return Ok(StartDecision::AtConcurrency);
        }
        if self.started >= self.limits.max_samples {
            self.stopped = Some(StopReason::SampleLimit);
            self.finalize_if_drained();
            return Ok(StartDecision::Stopped(StopReason::SampleLimit));
        }
        self.started += 1;
        self.in_flight += 1;
        if self.budget.requests == Some(self.started) {
            self.stopped = Some(StopReason::RequestBudget);
        }
        Ok(StartDecision::Start)
    }

    /// Observe a monotonic time event, including while requests are draining.
    pub fn observe(&mut self, now: std::time::Instant) -> Result<(), Error> {
        if now < self.last_checked {
            return Err(Error("non-monotonic start time"));
        }
        self.last_checked = now;
        if self.terminal.is_some() {
            return Ok(());
        }
        let elapsed = now.duration_since(self.started_at);
        if elapsed >= self.limits.max_duration && self.stopped != Some(StopReason::Cancelled) {
            self.stopped = Some(StopReason::AgentDeadline);
        } else if self.stopped.is_none() && self.budget.duration.is_some_and(|d| elapsed >= d) {
            self.stopped = Some(StopReason::Duration);
        }
        self.finalize_if_drained();
        Ok(())
    }

    /// Cancellation stops admission; outstanding operations must still be joined.
    pub fn cancel(&mut self, now: std::time::Instant) -> Result<(), Error> {
        if now < self.last_checked {
            return Err(Error("non-monotonic start time"));
        }
        self.last_checked = now;
        if self.terminal.is_some() {
            return Ok(());
        }
        self.stopped = Some(if self.stopped == Some(StopReason::Cancelled) {
            StopReason::Cancelled
        } else if now.duration_since(self.started_at) >= self.limits.max_duration {
            StopReason::AgentDeadline
        } else {
            StopReason::Cancelled
        });
        self.finalize_if_drained();
        Ok(())
    }

    /// Release a terminated operation, including one cancelled by the run.
    pub fn complete_at(&mut self, now: std::time::Instant) -> Result<(), Error> {
        if self.in_flight == 0 {
            return Err(Error("no request in flight"));
        }
        self.observe(now)?;
        self.in_flight = self
            .in_flight
            .checked_sub(1)
            .ok_or(Error("no request in flight"))?;
        self.finalize_if_drained();
        Ok(())
    }

    fn finalize_if_drained(&mut self) {
        if self.in_flight == 0 && self.terminal.is_none() {
            self.terminal = self.stopped;
        }
    }

    /// A terminal result is available only after all started requests finish.
    pub fn outcome(&self) -> Option<StopReason> {
        self.terminal
    }

    pub(crate) fn stop_reason(&self) -> Option<StopReason> {
        self.terminal.or(self.stopped)
    }

    /// Counts actual admissions, not rate-limiter reservations.
    pub fn started(&self) -> u64 {
        self.started
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight
    }
}

/// Split budgets equally in the supplied agent order; unused agents receive no work.
/// Capacity checks and enforcement of duration-only sample ceilings belong to the agent.
pub fn split_budget(budget: Budget, agents: usize) -> Result<Vec<Budget>, Error> {
    if agents == 0
        || budget.concurrency == 0
        || budget.rate_per_second == 0
        || budget.requests == Some(0)
        || budget.duration == Some(std::time::Duration::ZERO)
        || (budget.requests.is_none() && budget.duration.is_none())
    {
        return Err(Error("invalid distributed budget"));
    }
    let active = u64::try_from(agents)
        .unwrap_or(u64::MAX)
        .min(budget.concurrency)
        .min(budget.rate_per_second)
        .min(budget.requests.unwrap_or(u64::MAX));
    let active = usize::try_from(active).map_err(|_| Error("too many agents"))?;
    let share = |n: u64, i: usize| n / active as u64 + u64::from((i as u64) < n % active as u64);
    Ok((0..active)
        .map(|i| Budget {
            requests: budget.requests.map(|n| share(n, i)),
            concurrency: share(budget.concurrency, i),
            rate_per_second: share(budget.rate_per_second, i),
            duration: budget.duration,
        })
        .collect())
}

/// A completed agent result or an explicit reason for its absence.
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    Complete {
        completed: u64,
        failed: u64,
        histogram: DistributedHistogram,
    },
    Failed,
    TimedOut,
    Cancelled,
    SampleLimit,
    AgentDeadline,
    Invalid,
}

/// Terminal state retained for each agent without retaining its full histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Complete,
    Failed,
    TimedOut,
    Cancelled,
    SampleLimit,
    AgentDeadline,
    Invalid,
}

/// A configured threshold must not pass when its aggregate is incomplete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThresholdResult {
    NotConfigured,
    NotEvaluable,
    Passed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThresholdResults {
    pub failure_rate: ThresholdResult,
    pub p95: ThresholdResult,
}

/// An offline aggregate. Pending agents are missing, and make it partial.
pub struct Aggregate {
    run_id: [u8; 16],
    agents: std::collections::BTreeMap<String, Option<AgentStatus>>,
    completed: u64,
    failed: u64,
    histogram: Option<DistributedHistogram>,
    metrics: ResultMetrics,
    elapsed: std::collections::BTreeMap<String, std::time::Duration>,
}
impl Aggregate {
    pub fn new(run_id: [u8; 16], agents: &[String]) -> Result<Self, Error> {
        let mut identities = std::collections::BTreeMap::new();
        for agent in agents {
            if agent.is_empty() || identities.insert(agent.clone(), None).is_some() {
                return Err(Error("invalid agent list"));
            }
        }
        if identities.is_empty() {
            return Err(Error("no agents"));
        }
        Ok(Self {
            run_id,
            agents: identities,
            completed: 0,
            failed: 0,
            histogram: None,
            metrics: ResultMetrics::default(),
            elapsed: std::collections::BTreeMap::new(),
        })
    }
    fn add(&mut self, run_id: [u8; 16], agent: &str, outcome: AgentOutcome) -> Result<(), Error> {
        if run_id != self.run_id {
            return Err(Error("wrong run identifier"));
        }
        match self.agents.get(agent) {
            None => return Err(Error("unknown agent")),
            Some(Some(_)) => return Err(Error("duplicate agent result")),
            Some(None) => {}
        }
        let result = if let AgentOutcome::Complete {
            completed,
            failed,
            histogram,
        } = outcome
        {
            let validate = || -> Result<(u64, u64), Error> {
                if failed > completed || histogram.count() != completed {
                    return Err(Error("inconsistent agent result"));
                }
                let total = self
                    .completed
                    .checked_add(completed)
                    .ok_or(Error("completed count overflow"))?;
                let failures = self
                    .failed
                    .checked_add(failed)
                    .ok_or(Error("failed count overflow"))?;
                Ok((total, failures))
            };
            let merged = validate().and_then(|(total, failures)| {
                match &mut self.histogram {
                    Some(current) => current.merge(&histogram)?,
                    None => self.histogram = Some(histogram),
                }
                self.completed = total;
                self.failed = failures;
                Ok(())
            });
            merged.map(|()| AgentStatus::Complete)
        } else {
            Ok(match outcome {
                AgentOutcome::Complete { .. } => unreachable!(),
                AgentOutcome::Failed => AgentStatus::Failed,
                AgentOutcome::TimedOut => AgentStatus::TimedOut,
                AgentOutcome::Cancelled => AgentStatus::Cancelled,
                AgentOutcome::SampleLimit => AgentStatus::SampleLimit,
                AgentOutcome::AgentDeadline => AgentStatus::AgentDeadline,
                AgentOutcome::Invalid => AgentStatus::Invalid,
            })
        };
        *self.agents.get_mut(agent).expect("agent checked above") =
            Some(result.as_ref().copied().unwrap_or(AgentStatus::Invalid));
        result.map(|_| ())
    }

    /// Decode and add a complete result, recording invalid input for a known agent.
    fn add_encoded(
        &mut self,
        run_id: [u8; 16],
        agent: &str,
        completed: u64,
        failed: u64,
        encoded: &[u8],
        limits: DecodeLimits,
    ) -> Result<(), Error> {
        if run_id != self.run_id {
            return Err(Error("wrong run identifier"));
        }
        match self.agents.get(agent) {
            None => return Err(Error("unknown agent")),
            Some(Some(_)) => return Err(Error("duplicate agent result")),
            Some(None) => {}
        }
        match decode_histogram(encoded, completed, limits) {
            Ok(histogram) => self.add(
                run_id,
                agent,
                AgentOutcome::Complete {
                    completed,
                    failed,
                    histogram,
                },
            ),
            Err(error) => {
                self.add(run_id, agent, AgentOutcome::Invalid)?;
                Err(error)
            }
        }
    }

    /// Add a completed result using only locally assigned limits.
    pub fn add_completed(
        &mut self,
        run_id: [u8; 16],
        agent: &str,
        result: CompletedResult,
        assigned: Budget,
        encoded: &[u8],
        mut limits: DecodeLimits,
    ) -> Result<(), Error> {
        if run_id != self.run_id {
            return Err(Error("wrong run identifier"));
        }
        match self.agents.get(agent) {
            None => return Err(Error("unknown agent")),
            Some(Some(_)) => return Err(Error("duplicate agent result")),
            Some(None) => {}
        }
        let checked = result
            .validate(assigned, limits.max_samples)
            .and_then(|()| self.metrics.checked_add(&result.metrics));
        let totals = match checked {
            Ok(totals) => totals,
            Err(error) => {
                self.add(run_id, agent, AgentOutcome::Invalid)?;
                return Err(error);
            }
        };
        limits.assigned_requests = match (limits.assigned_requests, assigned.requests) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.add_encoded(
            run_id,
            agent,
            result.metrics.completed,
            result.metrics.failed,
            encoded,
            limits,
        )?;
        self.metrics = totals;
        self.elapsed.insert(agent.to_owned(), result.elapsed);
        Ok(())
    }

    /// Add the terminal outcome of an in-process Grunt using its assigned budget
    /// and receiver limits. Completed runs use the same validation as encoded results.
    pub fn add_local_outcome(
        &mut self,
        run_id: [u8; 16],
        agent: &str,
        outcome: LocalOutcome,
        assigned: Budget,
        limits: DecodeLimits,
    ) -> Result<(), Error> {
        match outcome {
            LocalOutcome::Complete {
                report,
                reason,
                started,
            } => {
                let prepared =
                    CompletedResult::from_local(&report, reason, started).and_then(|result| {
                        let encoded = encode_histogram(
                            report.bounded_latencies.as_ref(),
                            result.metrics.completed,
                        )?;
                        Ok((result, encoded))
                    });
                match prepared {
                    Ok((result, encoded)) => {
                        self.add_completed(run_id, agent, result, assigned, &encoded, limits)
                    }
                    Err(error) => {
                        self.add_noncomplete(run_id, agent, AgentOutcome::Invalid)?;
                        Err(error)
                    }
                }
            }
            LocalOutcome::Partial { reason } => {
                let status = match reason {
                    StopReason::Cancelled => AgentOutcome::Cancelled,
                    StopReason::AgentDeadline => AgentOutcome::AgentDeadline,
                    StopReason::SampleLimit => AgentOutcome::SampleLimit,
                    StopReason::RequestBudget | StopReason::Duration => {
                        self.add_noncomplete(run_id, agent, AgentOutcome::Invalid)?;
                        return Err(Error("invalid partial local stop reason"));
                    }
                };
                self.add_noncomplete(run_id, agent, status)
            }
        }
    }

    /// Record a terminal failure without accepting a partial metrics snapshot.
    pub fn add_noncomplete(
        &mut self,
        run_id: [u8; 16],
        agent: &str,
        outcome: AgentOutcome,
    ) -> Result<(), Error> {
        if matches!(outcome, AgentOutcome::Complete { .. }) {
            return Err(Error("complete result requires validation"));
        }
        self.add(run_id, agent, outcome)
    }

    pub fn metrics(&self) -> &ResultMetrics {
        &self.metrics
    }
    pub fn agent_elapsed(&self, agent: &str) -> Option<std::time::Duration> {
        self.elapsed.get(agent).copied()
    }
    pub fn is_partial(&self) -> bool {
        self.agents
            .values()
            .any(|v| !matches!(v, Some(AgentStatus::Complete)))
    }
    pub fn completed(&self) -> u64 {
        self.completed
    }
    pub fn failed(&self) -> u64 {
        self.failed
    }
    pub fn histogram(&self) -> Option<&DistributedHistogram> {
        self.histogram.as_ref()
    }

    /// Evaluate configured thresholds only for a complete, nonempty aggregate.
    pub fn evaluate_thresholds(
        &self,
        max_failure_rate: Option<f64>,
        max_p95_ms: Option<f64>,
    ) -> Result<ThresholdResults, Error> {
        if max_failure_rate.is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
            || max_p95_ms.is_some_and(|v| !v.is_finite() || v < 0.0)
        {
            return Err(Error("invalid threshold"));
        }
        let evaluate = |limit: Option<f64>, observed: Option<f64>| match limit {
            None => ThresholdResult::NotConfigured,
            Some(_) if self.is_partial() || self.completed == 0 => ThresholdResult::NotEvaluable,
            Some(limit) => match observed {
                None => ThresholdResult::NotEvaluable,
                Some(value) if value <= limit => ThresholdResult::Passed,
                Some(_) => ThresholdResult::Failed,
            },
        };
        Ok(ThresholdResults {
            failure_rate: evaluate(
                max_failure_rate,
                (self.completed > 0).then(|| self.failed as f64 / self.completed as f64 * 100.0),
            ),
            p95: evaluate(
                max_p95_ms,
                self.histogram
                    .as_ref()
                    .and_then(|h| h.percentile(95))
                    .map(|v| v.as_secs_f64() * 1000.0),
            ),
        })
    }
    pub fn agents(&self) -> &std::collections::BTreeMap<String, Option<AgentStatus>> {
        &self.agents
    }
    /// Uses the controller's measured window, never an average of agent rates.
    pub fn throughput(&self, controller_window: std::time::Duration) -> Option<f64> {
        (!controller_window.is_zero())
            .then(|| self.completed as f64 / controller_window.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn split_preserves_budgets_and_never_sends_zero() {
        let budget = Budget {
            requests: Some(100),
            concurrency: 7,
            rate_per_second: 10,
            duration: Some(Duration::from_secs(1)),
        };
        let parts = split_budget(budget, 3).unwrap();
        assert_eq!(
            parts
                .iter()
                .map(|p| p.requests.unwrap())
                .collect::<Vec<_>>(),
            [34, 33, 33]
        );
        assert_eq!(parts.iter().map(|p| p.concurrency).sum::<u64>(), 7);
        assert_eq!(parts.iter().map(|p| p.rate_per_second).sum::<u64>(), 10);
        assert_eq!(
            split_budget(
                Budget {
                    rate_per_second: 1,
                    ..budget
                },
                3
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            split_budget(
                Budget {
                    rate_per_second: 0,
                    ..budget
                },
                3
            )
            .is_err()
        );
        assert!(
            split_budget(
                Budget {
                    requests: None,
                    duration: None,
                    ..budget
                },
                3
            )
            .is_err()
        );
        assert_eq!(
            split_budget(
                Budget {
                    requests: None,
                    ..budget
                },
                3
            )
            .unwrap()[0]
                .requests,
            None
        );
        let limits = AgentLimits {
            max_samples: 33,
            max_concurrency: 3,
            max_rate_per_second: 4,
            max_duration: Duration::from_secs(2),
        };
        assert!(limits.validate(parts[0]).is_err());
        assert!(limits.validate(parts[1]).is_ok());
        let pre = preflight(parts[1], limits, Duration::from_millis(500)).unwrap();
        assert!(!pre.drain_may_hit_deadline.unwrap());
        let pre = preflight(parts[1], limits, Duration::from_secs(1)).unwrap();
        assert!(pre.drain_may_hit_deadline.unwrap());
        assert!(preflight(parts[1], limits, Duration::MAX).is_err());
        let permissive = AgentLimits {
            max_samples: u64::MAX,
            max_concurrency: u64::MAX,
            max_rate_per_second: u64::MAX,
            max_duration: Duration::MAX,
        };
        assert!(
            permissive
                .validate(Budget {
                    requests: None,
                    ..budget
                })
                .is_err()
        );
        assert!(
            permissive
                .validate(Budget {
                    rate_per_second: 1_000_000_001,
                    ..budget
                })
                .is_err()
        );
        assert!(
            permissive
                .validate(Budget {
                    rate_per_second: 1_000_000_000,
                    ..budget
                })
                .is_ok()
        );
    }

    #[test]
    fn start_gate_enforces_limits_and_drains_in_flight() {
        let now = std::time::Instant::now();
        let budget = Budget {
            requests: None,
            concurrency: 2,
            rate_per_second: 1,
            duration: Some(Duration::from_secs(10)),
        };
        let limits = AgentLimits {
            max_samples: 2,
            max_concurrency: 2,
            max_rate_per_second: 1,
            max_duration: Duration::from_secs(11),
        };
        let mut gate = StartGate::new(budget, limits, now).unwrap();
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::Start);
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::Start);
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::AtConcurrency);
        gate.complete_at(now).unwrap();
        assert_eq!(
            gate.try_start(now).unwrap(),
            StartDecision::Stopped(StopReason::SampleLimit)
        );
        assert_eq!(gate.outcome(), None);
        gate.complete_at(now).unwrap();
        assert!(gate.complete_at(now).is_err());
        assert_eq!(gate.outcome(), Some(StopReason::SampleLimit));
        assert!(gate.outcome().unwrap().is_partial());

        let mut gate = StartGate::new(budget, limits, now).unwrap();
        gate.try_start(now).unwrap();
        gate.try_start(now).unwrap();
        assert_eq!(
            gate.try_start(now + Duration::from_secs(10)).unwrap(),
            StartDecision::Stopped(StopReason::Duration)
        );
        gate.complete_at(now + Duration::from_secs(10)).unwrap();
        gate.complete_at(now + Duration::from_secs(10)).unwrap();
        assert!(!gate.outcome().unwrap().is_partial());

        let mut gate = StartGate::new(
            Budget {
                requests: Some(2),
                duration: None,
                ..budget
            },
            limits,
            now,
        )
        .unwrap();
        assert_eq!(
            gate.try_start(now + Duration::from_secs(11)).unwrap(),
            StartDecision::Stopped(StopReason::AgentDeadline)
        );
        assert!(gate.outcome().unwrap().is_partial());

        let mut gate = StartGate::new(
            Budget {
                requests: Some(2),
                duration: None,
                ..budget
            },
            limits,
            now,
        )
        .unwrap();
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::Start);
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::Start);
        assert_eq!(
            gate.try_start(now).unwrap(),
            StartDecision::Stopped(StopReason::RequestBudget)
        );
        gate.complete_at(now).unwrap();
        gate.complete_at(now).unwrap();
        assert!(!gate.outcome().unwrap().is_partial());

        let mut gate = StartGate::new(
            budget,
            AgentLimits {
                max_samples: 3,
                ..limits
            },
            now,
        )
        .unwrap();
        gate.try_start(now).unwrap();
        gate.try_start(now).unwrap();
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::AtConcurrency);
        gate.complete_at(now).unwrap();
        assert_eq!(
            gate.try_start(now + Duration::from_secs(10)).unwrap(),
            StartDecision::Stopped(StopReason::Duration)
        );
        assert!(gate.try_start(now).is_err());
    }

    #[test]
    fn deadlines_and_cancellation_override_draining_until_finalized() {
        let now = std::time::Instant::now();
        let budget = Budget {
            requests: Some(1),
            concurrency: 1,
            rate_per_second: 1,
            duration: Some(Duration::from_secs(3)),
        };
        let limits = AgentLimits {
            max_samples: 10,
            max_concurrency: 1,
            max_rate_per_second: 1,
            max_duration: Duration::from_secs(5),
        };
        assert!(
            StartGate::new(
                Budget {
                    duration: Some(Duration::from_secs(5)),
                    ..budget
                },
                limits,
                now
            )
            .is_err()
        );
        assert!(
            StartGate::new(
                budget,
                AgentLimits {
                    max_duration: Duration::MAX,
                    ..limits
                },
                now
            )
            .is_err()
        );
        let mut gate = StartGate::new(budget, limits, now).unwrap();
        assert_eq!(gate.try_start(now).unwrap(), StartDecision::Start);
        gate.observe(now + Duration::from_secs(5)).unwrap();
        assert_eq!(gate.outcome(), None);
        assert_eq!(
            gate.try_start(now + Duration::from_secs(5)).unwrap(),
            StartDecision::Stopped(StopReason::AgentDeadline)
        );
        gate.complete_at(now + Duration::from_secs(5)).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::AgentDeadline));

        let mut gate = StartGate::new(budget, limits, now).unwrap();
        gate.try_start(now).unwrap();
        gate.cancel(now + Duration::from_secs(1)).unwrap();
        gate.cancel(now + Duration::from_secs(2)).unwrap();
        assert_eq!(
            gate.try_start(now + Duration::from_secs(2)).unwrap(),
            StartDecision::Stopped(StopReason::Cancelled)
        );
        gate.observe(now + Duration::from_secs(5)).unwrap();
        assert_eq!(
            gate.try_start(now + Duration::from_secs(5)).unwrap(),
            StartDecision::Stopped(StopReason::Cancelled)
        );
        gate.complete_at(now + Duration::from_secs(5)).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::Cancelled));
        gate.observe(now + Duration::from_secs(5)).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::Cancelled));

        let mut gate = StartGate::new(budget, limits, now).unwrap();
        gate.try_start(now).unwrap();
        gate.complete_at(now + Duration::from_secs(1)).unwrap();
        gate.cancel(now + Duration::from_secs(2)).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::RequestBudget));

        let mut gate = StartGate::new(budget, limits, now).unwrap();
        gate.cancel(now).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::Cancelled));

        let mut gate = StartGate::new(budget, limits, now).unwrap();
        gate.cancel(now + Duration::from_secs(5)).unwrap();
        assert_eq!(gate.outcome(), Some(StopReason::AgentDeadline));
    }

    #[test]
    fn concurrent_admissions_do_not_exceed_the_request_budget() {
        use std::sync::{Arc, Barrier, Mutex};
        let now = std::time::Instant::now();
        let budget = Budget {
            requests: Some(10),
            concurrency: 20,
            rate_per_second: 10,
            duration: None,
        };
        let limits = AgentLimits {
            max_samples: 10,
            max_concurrency: 20,
            max_rate_per_second: 10,
            max_duration: Duration::from_secs(5),
        };
        let gate = Arc::new(Mutex::new(StartGate::new(budget, limits, now).unwrap()));
        let barrier = Arc::new(Barrier::new(32));
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let (gate, barrier) = (Arc::clone(&gate), Arc::clone(&barrier));
                scope.spawn(move || {
                    barrier.wait();
                    gate.lock().unwrap().try_start(now).unwrap();
                });
            }
        });
        let mut gate = gate.lock().unwrap();
        assert_eq!(gate.started(), 10);
        assert_eq!(gate.in_flight(), 10);
        gate.cancel(now).unwrap();
        assert_eq!(
            gate.try_start(now).unwrap(),
            StartDecision::Stopped(StopReason::Cancelled)
        );
        for _ in 0..10 {
            gate.complete_at(now).unwrap();
        }
        assert_eq!(gate.outcome(), Some(StopReason::Cancelled));
    }
    #[test]
    fn aggregate_rejects_duplicates_and_marks_missing_and_failed_partial() {
        let ids = vec!["one".to_string(), "two".to_string()];
        let mut aggregate = Aggregate::new([1; 16], &ids).unwrap();
        assert!(aggregate.is_partial());
        assert!(aggregate.add([2; 16], "one", AgentOutcome::Failed).is_err());
        assert!(
            aggregate
                .add([1; 16], "other", AgentOutcome::Failed)
                .is_err()
        );
        let encoded = encode_histogram(None, 0).unwrap();
        let histogram = decode_histogram(
            &encoded,
            0,
            DecodeLimits {
                max_envelope_bytes: MAX_ENVELOPE_BYTES,
                max_samples: 100,
                assigned_requests: None,
            },
        )
        .unwrap();
        aggregate
            .add(
                [1; 16],
                "one",
                AgentOutcome::Complete {
                    completed: 0,
                    failed: 0,
                    histogram,
                },
            )
            .unwrap();
        assert!(aggregate.add([1; 16], "one", AgentOutcome::Failed).is_err());
        aggregate
            .add([1; 16], "two", AgentOutcome::TimedOut)
            .unwrap();
        assert!(aggregate.is_partial());
        assert_eq!(aggregate.throughput(Duration::ZERO), None);
        assert_eq!(aggregate.throughput(Duration::from_secs(2)), Some(0.0));
    }

    #[test]
    fn invalid_result_marks_known_agent_and_thresholds_are_never_passing_when_partial() {
        let ids = vec!["one".to_string(), "two".to_string()];
        let mut collector = crate::BoundedLatency::new();
        collector.record(Duration::from_millis(10)).unwrap();
        let bytes = encode_histogram(Some(&collector), 1).unwrap();
        let histogram = decode_histogram(
            &bytes,
            1,
            DecodeLimits {
                max_envelope_bytes: MAX_ENVELOPE_BYTES,
                max_samples: 10,
                assigned_requests: None,
            },
        )
        .unwrap();
        let mut aggregate = Aggregate::new([1; 16], &ids).unwrap();
        aggregate
            .add(
                [1; 16],
                "one",
                AgentOutcome::Complete {
                    completed: 1,
                    failed: 0,
                    histogram: histogram.clone(),
                },
            )
            .unwrap();
        assert!(
            aggregate
                .add(
                    [2; 16],
                    "two",
                    AgentOutcome::Complete {
                        completed: 2,
                        failed: 0,
                        histogram: histogram.clone()
                    }
                )
                .is_err()
        );
        assert_eq!(aggregate.agents()["two"], None);
        assert!(
            aggregate
                .add(
                    [1; 16],
                    "two",
                    AgentOutcome::Complete {
                        completed: 2,
                        failed: 0,
                        histogram
                    }
                )
                .is_err()
        );
        assert_eq!(aggregate.agents()["two"], Some(AgentStatus::Invalid));
        assert_eq!(aggregate.completed(), 1);
        let results = aggregate
            .evaluate_thresholds(Some(1.0), Some(20.0))
            .unwrap();
        assert_eq!(results.failure_rate, ThresholdResult::NotEvaluable);
        assert_eq!(results.p95, ThresholdResult::NotEvaluable);
        assert_eq!(
            aggregate.evaluate_thresholds(None, None).unwrap().p95,
            ThresholdResult::NotConfigured
        );
        assert!(aggregate.evaluate_thresholds(Some(f64::NAN), None).is_err());
    }

    #[test]
    fn complete_aggregate_evaluates_thresholds_and_empty_is_not_evaluable() {
        let ids = vec!["one".to_string()];
        let limits = DecodeLimits {
            max_envelope_bytes: MAX_ENVELOPE_BYTES,
            max_samples: 10,
            assigned_requests: None,
        };
        let mut empty = Aggregate::new([0; 16], &ids).unwrap();
        let histogram = decode_histogram(&encode_histogram(None, 0).unwrap(), 0, limits).unwrap();
        empty
            .add(
                [0; 16],
                "one",
                AgentOutcome::Complete {
                    completed: 0,
                    failed: 0,
                    histogram,
                },
            )
            .unwrap();
        assert_eq!(
            empty
                .evaluate_thresholds(Some(1.0), None)
                .unwrap()
                .failure_rate,
            ThresholdResult::NotEvaluable
        );
        let mut h = crate::BoundedLatency::new();
        h.record(Duration::from_millis(10)).unwrap();
        let histogram =
            decode_histogram(&encode_histogram(Some(&h), 1).unwrap(), 1, limits).unwrap();
        let mut aggregate = Aggregate::new([0; 16], &ids).unwrap();
        aggregate
            .add(
                [0; 16],
                "one",
                AgentOutcome::Complete {
                    completed: 1,
                    failed: 1,
                    histogram,
                },
            )
            .unwrap();
        let result = aggregate
            .evaluate_thresholds(Some(99.0), Some(10.0))
            .unwrap();
        assert_eq!(result.failure_rate, ThresholdResult::Failed);
        assert_eq!(result.p95, ThresholdResult::Passed);
    }

    #[test]
    fn invalid_encoded_result_is_recorded_only_for_matching_run_and_agent() {
        let ids = vec!["one".to_string()];
        let mut aggregate = Aggregate::new([1; 16], &ids).unwrap();
        let limits = DecodeLimits {
            max_envelope_bytes: MAX_ENVELOPE_BYTES,
            max_samples: 10,
            assigned_requests: None,
        };
        assert!(
            aggregate
                .add_encoded([2; 16], "one", 0, 0, &[], limits)
                .is_err()
        );
        assert!(
            aggregate
                .add_encoded([1; 16], "other", 0, 0, &[], limits)
                .is_err()
        );
        assert_eq!(aggregate.agents()["one"], None);
        assert!(
            aggregate
                .add_encoded([1; 16], "one", 0, 0, &[], limits)
                .is_err()
        );
        assert_eq!(aggregate.agents()["one"], Some(AgentStatus::Invalid));
    }

    #[test]
    fn completed_results_aggregate_transactionally_and_invalid_agent_is_partial() {
        let ids = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let mut aggregate = Aggregate::new([9; 16], &ids).unwrap();
        let budget = Budget {
            requests: Some(1),
            concurrency: 1,
            rate_per_second: 1,
            duration: None,
        };
        let limits = DecodeLimits {
            max_envelope_bytes: MAX_ENVELOPE_BYTES,
            max_samples: 1,
            assigned_requests: Some(1),
        };
        let mut histogram = crate::BoundedLatency::new();
        histogram.record(Duration::from_millis(10)).unwrap();
        let encoded = encode_histogram(Some(&histogram), 1).unwrap();
        let result = CompletedResult {
            version: 1,
            reason: CompletionReason::RequestBudget,
            elapsed: Duration::from_secs(2),
            metrics: ResultMetrics {
                started: 1,
                completed: 1,
                failed: 1,
                http_failures: 1,
                timeout_failures: 1,
                body_failures: 1,
                response_bytes: 50,
                connections_opened: 2,
                redirects_followed: 3,
                status_codes: std::collections::BTreeMap::from([(500, 1)]),
                ..Default::default()
            },
        };
        aggregate
            .add_completed([9; 16], "one", result.clone(), budget, &encoded, limits)
            .unwrap();
        aggregate
            .add_completed([9; 16], "two", result.clone(), budget, &encoded, limits)
            .unwrap();
        assert_eq!(aggregate.metrics().status_codes[&500], 2);
        assert_eq!(aggregate.metrics().response_bytes, 100);
        assert_eq!(aggregate.agent_elapsed("one"), Some(Duration::from_secs(2)));
        let before = aggregate.metrics().clone();
        let mut bad = result;
        bad.metrics.response_bytes = u64::MAX;
        assert!(
            aggregate
                .add_completed([9; 16], "three", bad, budget, &encoded, limits)
                .is_err()
        );
        assert_eq!(aggregate.metrics(), &before);
        assert_eq!(aggregate.completed(), 2);
        assert_eq!(aggregate.histogram().unwrap().count(), 2);
        assert_eq!(aggregate.agents()["three"], Some(AgentStatus::Invalid));
        assert!(aggregate.is_partial());
        assert_eq!(
            aggregate
                .evaluate_thresholds(Some(100.0), None)
                .unwrap()
                .failure_rate,
            ThresholdResult::NotEvaluable
        );
    }
}
