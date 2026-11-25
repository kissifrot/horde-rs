use super::{BoxError, DEFAULT_PROFILE_LIMITS, ReportFormat, agent_status};
use crate::{
    signals::Interrupt,
    text_report::{TextReport, Tone},
};
use horde_rs::distributed::{
    controller::{Grunt, ScenarioControllerReport, ScenarioMeasurement, ScenarioPreflight},
    transport::{MAX_SCENARIO_BYTES, ScenarioProfile},
};
use serde_json::json;
use std::{
    collections::BTreeMap, fs::File, io::Read, path::PathBuf, process::ExitCode, time::Duration,
};

pub(super) fn load(
    paths: BTreeMap<String, PathBuf>,
    user_agent: Option<String>,
) -> Result<BTreeMap<String, ScenarioProfile>, BoxError> {
    if paths.len() > 16 {
        return Err("configure at most 16 scenarios per Grunt".into());
    }
    paths
        .into_iter()
        .map(|(name, path)| {
            let load = || -> Result<ScenarioProfile, BoxError> {
                let mut input = Vec::new();
                File::open(&path)?
                    .take(MAX_SCENARIO_BYTES as u64 + 1)
                    .read_to_end(&mut input)?;
                ScenarioProfile::from_json(
                    &input,
                    DEFAULT_PROFILE_LIMITS,
                    Duration::from_secs(10),
                    user_agent.clone(),
                )
            };
            let profile = load()
                .map_err(|error| format!("scenario {name:?} in {}: {error}", path.display()))?;
            Ok((name, profile))
        })
        .collect()
}

pub(super) fn print_report(
    name: &str,
    report: ScenarioControllerReport,
    format: ReportFormat,
    max_failure_rate: Option<f64>,
    max_p95: Option<f64>,
    signal: Option<Interrupt>,
) -> Result<ExitCode, BoxError> {
    let partial = report.is_partial() || signal.is_some();
    let completed = report.totals.completed;
    let failed = report.totals.failed;
    let failure_rate = (completed != 0).then(|| failed as f64 / completed as f64 * 100.0);
    let p95 = percentile(&report.totals, 95);
    let threshold = |limit: Option<f64>, observed| match (limit, observed) {
        (None, _) => "not_configured",
        (Some(_), _) if partial => "not_evaluable",
        (Some(limit), Some(value)) if value <= limit => "passed",
        _ => "failed",
    };
    let failure_threshold = threshold(max_failure_rate, failure_rate);
    let p95_threshold = threshold(max_p95, p95);
    let thresholds_failed = [failure_threshold, p95_threshold].contains(&"failed");
    let elapsed = report.elapsed.as_secs_f64();
    let throughput = (elapsed > 0.0).then(|| completed as f64 / elapsed);
    match format {
        ReportFormat::Json => {
            let steps = report
                .steps
                .iter()
                .map(|(name, step)| {
                    json!({
                        "name": name, "completed": step.completed, "failed": step.failed,
                        "skipped": completed - step.completed, "latency_ms": percentiles(step),
                    })
                })
                .collect::<Vec<_>>();
            let agents = report.agents.iter().map(|agent| json!({
                "address": agent.address, "status": agent_status(agent.status).0, "reason": agent.rejection,
            })).collect::<Vec<_>>();
            println!(
                "{}",
                json!({
                    "schema_version": 1, "type": "distributed_scenario", "name": name,
                    "partial": partial, "interruption": signal.map(Interrupt::label),
                    "elapsed_seconds": elapsed, "latency_method": "hdr_histogram_upper_bound_3_significant_digits",
                    "scenarios": {
                        "completed": completed, "failed": failed, "completed_per_second": throughput,
                        "duration_ms": percentiles(&report.totals),
                    },
                    "steps": steps, "agents": agents,
                    "thresholds": {"failure_rate": failure_threshold, "p95": p95_threshold},
                })
            );
        }
        ReportFormat::Text => {
            let (status, tone) = if partial {
                ("PARTIAL", Tone::Warning)
            } else if failed > 0 || thresholds_failed {
                ("FAIL", Tone::Error)
            } else {
                ("PASS", Tone::Good)
            };
            let mut output = TextReport::new("Distributed scenario", status, tone);
            output.field("Scenario:", name);
            if let Some(signal) = signal {
                output.field("Interruption:", signal.label());
            }
            output.field("Completed iterations:", completed);
            output.field("Failed iterations:", failed);
            if let Some(p95) = p95 {
                output.field("Iteration p95:", format_args!("{p95:.2} ms"));
            }
            output.section("Steps");
            for (name, step) in &report.steps {
                output.line(format_args!(
                    "  {name}: {} completed, {} failed, {} skipped",
                    step.completed,
                    step.failed,
                    completed - step.completed,
                ));
            }
            output.section("Grunts");
            for agent in &report.agents {
                let (_, label, tone) = agent_status(agent.status);
                let label = output.status(label, tone);
                output.line(format_args!("  {}  {label}", agent.address));
            }
            if max_failure_rate.is_some() || max_p95.is_some() {
                output.section("CI thresholds");
                output.field("Failure rate:", failure_threshold);
                output.field("Iteration p95:", p95_threshold);
            }
            output.print()?;
        }
    }
    Ok(ExitCode::from(signal.map_or(
        u8::from(partial || thresholds_failed),
        Interrupt::code,
    )))
}

fn percentile(measurement: &ScenarioMeasurement, percent: u8) -> Option<f64> {
    measurement
        .histogram
        .as_ref()?
        .percentile(percent)
        .map(|duration| duration.as_secs_f64() * 1000.0)
}

fn percentiles(measurement: &ScenarioMeasurement) -> serde_json::Value {
    json!({
        "p50": percentile(measurement, 50),
        "p95": percentile(measurement, 95),
        "p99": percentile(measurement, 99),
    })
}
pub(super) fn preflight_report(
    name: &str,
    grunts: &[Grunt],
    results: Vec<Result<ScenarioPreflight, BoxError>>,
    signal: Option<Interrupt>,
) -> (bool, serde_json::Value) {
    let reference = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .map(|peer| peer.description.clone());
    let (checks, revisions): (Vec<_>, Vec<_>) = results.into_iter().map(|result| {
        match result {
            Ok(peer) => {
                let revision = peer.description.revision.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
                let check = if reference.as_ref() == Some(&peer.description) {
                    Ok(peer.epoch)
                } else {
                    Err("scenario revision or step list differs from the first Grunt with the scenario".into())
                };
                (check, Some(revision))
            }
            Err(error) => (Err(error), None),
        }
    }).unzip();
    let (ready, mut report) = super::preflight_report(grunts, checks, signal);
    report["scenario"] = json!(name);
    let peers = report["grunts"]
        .as_array_mut()
        .expect("preflight Grunt results");
    for (peer, revision) in peers.iter_mut().zip(revisions) {
        peer["scenario_revision"] = json!(revision);
    }
    (ready, report)
}
