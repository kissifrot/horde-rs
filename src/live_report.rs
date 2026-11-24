use crate::{
    signals::{CLEAR_LINE, Interrupt},
    text_report::{TextReport, Tone},
};
use horde_rs::{ProgressSnapshot, RunProgress};
use std::{
    fmt,
    future::Future,
    io::{self, IsTerminal, Write as _},
    process::ExitCode,
    time::Duration,
};
use tokio::{
    sync::watch,
    time::{Instant, MissedTickBehavior, interval_at},
};

pub(crate) struct LiveReport {
    pub(crate) progress: Option<RunProgress>,
    pub(crate) live: bool,
    scenario: bool,
}

impl LiveReport {
    pub(crate) fn new(text_run: bool, verbose: bool, scenario: bool) -> Self {
        Self {
            progress: text_run.then(RunProgress::default),
            live: text_run
                && !verbose
                && io::stdout().is_terminal()
                && io::stderr().is_terminal()
                && std::env::var_os("TERM").is_none_or(|value| value != "dumb"),
            scenario,
        }
    }

    pub(crate) async fn monitor<T>(
        &self,
        interrupts: &watch::Receiver<Option<Interrupt>>,
        run: impl Future<Output = T>,
    ) -> T {
        if !self.live {
            return run.await;
        }
        let interval = Duration::from_secs(1);
        let mut timer = interval_at(Instant::now() + interval, interval);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tokio::pin!(run);
        let mut drawn = false;
        let result = loop {
            tokio::select! {
                biased;
                result = &mut run => break result,
                _ = timer.tick() => {
                    if let Some(snapshot) = self.progress.as_ref().and_then(RunProgress::snapshot) {
                        let rate = rate(&snapshot);
                        let unit = if self.scenario { "iter/s" } else { "req/s" };
                        drawn |= terminal_line(interrupts, format_args!(
                            "Zug zug! {} done | {} failed | {rate:.1} {unit}",
                            snapshot.completed, snapshot.failed
                        ));
                    }
                }
            }
        };
        if drawn {
            terminal_line(interrupts, format_args!(""));
        }
        result
    }

    pub(crate) fn interrupted(
        &self,
        interrupts: &watch::Receiver<Option<Interrupt>>,
        json: bool,
        bounded_latency: bool,
    ) -> io::Result<ExitCode> {
        let Some(progress) = &self.progress else {
            return Ok(crate::signals::report(interrupts, json));
        };
        let signal = interrupts.borrow().unwrap_or(Interrupt::Interrupt);
        let snapshot = progress.snapshot().unwrap_or_default();
        let mut report = TextReport::new("Standalone", "PARTIAL", Tone::Warning);
        report.line(format_args!("Run interrupted ({}).", signal.label()));
        let (heading, completed, failed, rate_label) = if self.scenario {
            (
                "Scenarios",
                "Completed scenarios:",
                "Failed scenarios:",
                "Completed/sec:",
            )
        } else {
            (
                "Requests",
                "Completed requests:",
                "Failed requests:",
                "Requests/sec:",
            )
        };
        report.section(heading);
        report.field(completed, snapshot.completed);
        report.field(failed, snapshot.failed);
        report.field(
            "Elapsed:",
            format_args!("{:.3} s", snapshot.elapsed.as_secs_f64()),
        );
        report.field(rate_label, format_args!("{:.2}", rate(&snapshot)));

        let (heading, prefix) = if self.scenario {
            ("Iteration duration", "Duration")
        } else {
            ("Latency", "Latency")
        };
        let method = if bounded_latency {
            "HDR histogram, approximate upper bounds"
        } else {
            "exact"
        };
        report.section(&format!("{heading} ({method})"));
        if let Some((p95, p99)) = progress.partial_percentiles() {
            for (percentile, duration) in [(95, p95), (99, p99)] {
                report.field(
                    &format!("{prefix} p{percentile}:"),
                    format_args!("{:.2} ms", duration.as_secs_f64() * 1000.0),
                );
            }
        } else {
            report.line(format_args!("No samples"));
        }
        report.line(format_args!("\nCI thresholds: not evaluable"));
        report.print()?;
        Ok(ExitCode::from(signal.code()))
    }
}

fn rate(snapshot: &ProgressSnapshot) -> f64 {
    if snapshot.elapsed.is_zero() {
        0.0
    } else {
        snapshot.completed as f64 / snapshot.elapsed.as_secs_f64()
    }
}

fn terminal_line(
    interrupts: &watch::Receiver<Option<Interrupt>>,
    line: fmt::Arguments<'_>,
) -> bool {
    let mut stderr = io::stderr().lock();
    // The signal handler clears the current line before printing its notice.
    if interrupts.borrow().is_some() {
        return false;
    }
    write!(stderr, "{CLEAR_LINE}{line}")
        .and_then(|()| stderr.flush())
        .is_ok()
}
