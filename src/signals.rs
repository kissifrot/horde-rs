//! CLI process interrupts; request workers are joined before a partial report is emitted.
use horde_rs::Cancellation;
use std::{io, process::ExitCode, time::Duration};
use tokio::sync::watch;

pub(crate) const CLEAR_LINE: &str = "\r\x1b[2K";

#[derive(Clone, Copy, Debug)]
pub enum Interrupt {
    Interrupt,
    Terminate,
}
impl Interrupt {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "sigint",
            Self::Terminate => "sigterm",
        }
    }
}

#[cfg(unix)]
pub fn install(cancel: Cancellation, live: bool) -> io::Result<watch::Receiver<Option<Interrupt>>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let first = tokio::select! { _ = interrupt.recv() => Interrupt::Interrupt, _ = terminate.recv() => Interrupt::Terminate };
        tx.send_replace(Some(first));
        cancel.cancel();
        notify(live);
        let exit = tokio::select! {
            _ = interrupt.recv() => Interrupt::Interrupt,
            _ = terminate.recv() => Interrupt::Terminate,
            _ = tokio::time::sleep(Duration::from_secs(5)) => first,
        };
        std::process::exit(i32::from(exit.code()));
    });
    Ok(rx)
}

#[cfg(not(unix))]
pub fn install(cancel: Cancellation, live: bool) -> io::Result<watch::Receiver<Option<Interrupt>>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tx.send_replace(Some(Interrupt::Interrupt));
        cancel.cancel();
        notify(live);
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
        std::process::exit(130);
    });
    Ok(rx)
}

fn notify(live: bool) {
    let prefix = if live { CLEAR_LINE } else { "" };
    eprintln!(
        "{prefix}Interrupt received; stopping active requests. Send another signal to stop immediately."
    );
}

pub fn report(signal: &watch::Receiver<Option<Interrupt>>, json: bool) -> ExitCode {
    let signal = signal.borrow().unwrap_or(Interrupt::Interrupt);
    if json {
        println!(
            "{}",
            serde_json::json!({"schema_version": 1, "type": "interrupted", "partial": true, "reason": signal.label(), "thresholds": {"status": "not_evaluable"}})
        );
    } else {
        println!(
            "For the Horde!\nRun interrupted ({}); partial results are unavailable.\nCI thresholds: not evaluable",
            signal.label()
        );
    }
    ExitCode::from(signal.code())
}
