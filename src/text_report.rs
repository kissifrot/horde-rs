use std::{
    fmt::{self, Write as _},
    io::{self, IsTerminal, Write as _},
};

pub(crate) enum Tone {
    Good,
    Warning,
    Error,
}

pub(crate) struct TextReport {
    buffer: String,
    color: bool,
}

impl TextReport {
    pub(crate) fn standalone(thresholds_failed: bool, has_failures: bool) -> Self {
        let (status, tone) = match (thresholds_failed, has_failures) {
            (true, _) => ("THRESHOLD FAILED", Tone::Error),
            (false, true) => ("COMPLETE WITH ERRORS", Tone::Warning),
            (false, false) => ("COMPLETE", Tone::Good),
        };
        Self::new("Standalone", status, tone)
    }

    pub(crate) fn new(mode: &str, status: &str, tone: Tone) -> Self {
        let mut report = Self {
            buffer: String::new(),
            color: io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
                && std::env::var_os("TERM").is_none_or(|value| value != "dumb"),
        };
        let status = report.status(status, tone);
        report.line(format_args!("For the Horde! | {mode} | {status}"));
        report
    }

    pub(crate) fn status(&self, label: &str, tone: Tone) -> String {
        if !self.color {
            return label.to_owned();
        }
        let color = match tone {
            Tone::Good => 32,
            Tone::Warning => 33,
            Tone::Error => 31,
        };
        format!("\x1b[1;{color}m{label}\x1b[0m")
    }

    pub(crate) fn verdict(&self, passed: bool) -> String {
        if passed {
            self.status("PASS", Tone::Good)
        } else {
            self.status("FAIL", Tone::Error)
        }
    }

    pub(crate) fn section(&mut self, heading: &str) {
        self.line(format_args!("\n{heading}"));
    }

    pub(crate) fn field(&mut self, label: &str, value: impl fmt::Display) {
        self.line(format_args!("{label:<25}{value}"));
    }

    pub(crate) fn line(&mut self, line: fmt::Arguments<'_>) {
        writeln!(self.buffer, "{line}").expect("writing to String cannot fail");
    }

    pub(crate) fn print(self) -> io::Result<()> {
        io::stdout().lock().write_all(self.buffer.as_bytes())
    }
}
