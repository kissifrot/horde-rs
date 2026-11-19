//! Logical completed-result validation. This is not a wire format.
use super::{Budget, Error, StopReason};
use crate::Report;
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    RequestBudget,
    Duration,
}

/// Counts for logical requests; failure categories may overlap.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultMetrics {
    pub started: u64,
    pub completed: u64,
    pub failed: u64,
    pub http_failures: u64,
    pub timeout_failures: u64,
    pub connection_failures: u64,
    pub body_failures: u64,
    pub redirect_limit_failures: u64,
    pub other_failures: u64,
    pub response_bytes: u64,
    pub connections_opened: u64,
    pub redirects_followed: u64,
    pub status_codes: BTreeMap<u16, u64>,
}

/// Metadata from an agent claiming that all its started requests have completed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedResult {
    pub version: u16,
    pub reason: CompletionReason,
    pub elapsed: Duration,
    pub metrics: ResultMetrics,
}

impl CompletedResult {
    pub(crate) fn from_local(
        report: &Report,
        reason: StopReason,
        started: u64,
    ) -> Result<Self, Error> {
        let reason = match reason {
            StopReason::RequestBudget => CompletionReason::RequestBudget,
            StopReason::Duration => CompletionReason::Duration,
            _ => return Err(Error("invalid complete local stop reason")),
        };
        let count =
            |value: usize| u64::try_from(value).map_err(|_| Error("local counter out of range"));
        Ok(Self {
            version: 1,
            reason,
            elapsed: report.elapsed,
            metrics: ResultMetrics {
                started,
                completed: count(report.completed)?,
                failed: count(report.failed)?,
                http_failures: count(report.http_failures)?,
                timeout_failures: count(report.timeout_failures)?,
                connection_failures: count(report.connection_failures)?,
                body_failures: count(report.body_failures)?,
                redirect_limit_failures: count(report.redirect_limit_failures)?,
                other_failures: count(report.other_failures)?,
                response_bytes: report.bytes,
                connections_opened: count(report.connections_opened)?,
                redirects_followed: count(report.redirects_followed)?,
                status_codes: report
                    .status_codes
                    .iter()
                    .map(|(&code, &value)| Ok((code, count(value)?)))
                    .collect::<Result<_, Error>>()?,
            },
        })
    }

    pub(crate) fn validate(&self, assigned: Budget, sample_limit: u64) -> Result<(), Error> {
        let m = &self.metrics;
        if sample_limit == 0
            || assigned.requests == Some(0)
            || assigned.concurrency == 0
            || assigned.rate_per_second == 0
            || assigned.duration == Some(Duration::ZERO)
            || (assigned.requests.is_none() && assigned.duration.is_none())
        {
            return Err(Error("invalid assigned budget"));
        }
        if self.version != 1 {
            return Err(Error("unsupported result version"));
        }
        if m.started != m.completed
            || m.failed > m.completed
            || m.started > sample_limit
            || assigned.requests.is_some_and(|n| m.started > n)
        {
            return Err(Error("inconsistent request counts"));
        }
        match self.reason {
            CompletionReason::RequestBudget
                if assigned.requests != Some(m.started) || m.started == 0 =>
            {
                return Err(Error("invalid completion reason"));
            }
            CompletionReason::Duration if assigned.duration.is_none() => {
                return Err(Error("invalid completion reason"));
            }
            _ => {}
        }
        let mut statuses = 0u64;
        let mut http = 0u64;
        if m.status_codes.len() > 900 {
            return Err(Error("too many status codes"));
        }
        for (&code, &count) in &m.status_codes {
            if !(100..=999).contains(&code) || count == 0 {
                return Err(Error("invalid status count"));
            }
            statuses = statuses
                .checked_add(count)
                .ok_or(Error("status count overflow"))?;
            if (400..=599).contains(&code) {
                http = http
                    .checked_add(count)
                    .ok_or(Error("status count overflow"))?;
            }
        }
        if statuses > m.completed || m.completed - statuses > m.failed || http != m.http_failures {
            return Err(Error("inconsistent status counts"));
        }
        let failures = [
            m.http_failures,
            m.timeout_failures,
            m.connection_failures,
            m.body_failures,
            m.redirect_limit_failures,
            m.other_failures,
        ];
        if failures.iter().any(|&v| v > m.failed)
            || failures.iter().map(|&v| u128::from(v)).sum::<u128>() < u128::from(m.failed)
        {
            return Err(Error("inconsistent failure counts"));
        }
        Ok(())
    }
}

impl ResultMetrics {
    /// Check the whole aggregate before returning it; the original stays intact on error.
    pub(crate) fn checked_add(&self, other: &Self) -> Result<Self, Error> {
        let mut sum = self.clone();
        macro_rules! add { ($($field:ident),+ $(,)?) => { $(sum.$field = sum.$field.checked_add(other.$field).ok_or(Error("result counter overflow"))?;)+ }; }
        add!(
            started,
            completed,
            failed,
            http_failures,
            timeout_failures,
            connection_failures,
            body_failures,
            redirect_limit_failures,
            other_failures,
            response_bytes,
            connections_opened,
            redirects_followed
        );
        for (&code, &count) in &other.status_codes {
            let value = sum.status_codes.entry(code).or_default();
            *value = value
                .checked_add(count)
                .ok_or(Error("status count overflow"))?;
        }
        Ok(sum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn budget() -> Budget {
        Budget {
            requests: Some(1),
            concurrency: 1,
            rate_per_second: 1,
            duration: Some(Duration::from_secs(1)),
        }
    }
    fn example() -> CompletedResult {
        CompletedResult {
            version: 1,
            reason: CompletionReason::RequestBudget,
            elapsed: Duration::from_secs(1),
            metrics: ResultMetrics {
                started: 1,
                completed: 1,
                failed: 1,
                http_failures: 1,
                timeout_failures: 1,
                body_failures: 1,
                status_codes: BTreeMap::from([(500, 1)]),
                ..Default::default()
            },
        }
    }
    #[test]
    fn validate_overlapping_failures_and_missing_status() {
        let mut r = example();
        assert!(r.validate(budget(), 1).is_ok());
        r.metrics.http_failures = 0;
        r.metrics.status_codes.clear();
        r.metrics.body_failures = 0;
        assert!(r.validate(budget(), 1).is_ok());
        r.metrics.timeout_failures = 0;
        assert!(r.validate(budget(), 1).is_err());
        r.metrics.failed = 0;
        assert!(r.validate(budget(), 1).is_err());
    }
    #[test]
    fn reject_inconsistent_result_fields() {
        let mut r = example();
        r.metrics.started = 0;
        assert!(r.validate(budget(), 1).is_err());
        let mut r = example();
        r.metrics.status_codes.insert(200, 1);
        assert!(r.validate(budget(), 1).is_err());
        let mut r = example();
        r.metrics.status_codes.insert(99, 1);
        assert!(r.validate(budget(), 1).is_err());
        let mut r = example();
        r.metrics.http_failures = 0;
        assert!(r.validate(budget(), 1).is_err());
        let mut r = example();
        r.metrics.status_codes.insert(200, 0);
        assert!(r.validate(budget(), 1).is_err());
        let mut r = example();
        r.version = 2;
        assert!(r.validate(budget(), 1).is_err());
        let r = example();
        assert!(r.validate(budget(), 0).is_err());
        let r = example();
        assert!(
            r.validate(
                Budget {
                    requests: Some(2),
                    ..budget()
                },
                2
            )
            .is_err()
        );
        let mut r = example();
        r.reason = CompletionReason::Duration;
        assert!(
            r.validate(
                Budget {
                    duration: None,
                    ..budget()
                },
                1
            )
            .is_err()
        );
    }
    #[test]
    fn checked_merge_is_transactional() {
        let original = ResultMetrics {
            response_bytes: u64::MAX,
            ..Default::default()
        };
        let other = ResultMetrics {
            response_bytes: 1,
            ..Default::default()
        };
        assert!(original.checked_add(&other).is_err());
        assert_eq!(original.response_bytes, u64::MAX);
    }

    #[test]
    fn duration_can_complete_empty_or_before_request_budget() {
        let result = CompletedResult {
            version: 1,
            reason: CompletionReason::Duration,
            elapsed: Duration::from_secs(1),
            metrics: ResultMetrics::default(),
        };
        assert!(result.validate(budget(), 1).is_ok());
        let mut partial_count = example();
        partial_count.reason = CompletionReason::Duration;
        assert!(
            partial_count
                .validate(
                    Budget {
                        requests: Some(2),
                        ..budget()
                    },
                    2
                )
                .is_ok()
        );
    }
}
