//! One-way cancellation shared by local execution modes.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[derive(Clone, Debug)]
pub struct Cancellation {
    sender: tokio::sync::watch::Sender<bool>,
    cancelled: Arc<AtomicBool>,
}
impl Default for Cancellation {
    fn default() -> Self {
        let (sender, _) = tokio::sync::watch::channel(false);
        Self {
            sender,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.send_replace(true);
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.sender.subscribe()
    }
}
