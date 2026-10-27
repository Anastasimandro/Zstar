use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A snapshot handed to the observer. Totals are known up front: extraction
/// sums the entry sizes, compression sums the input file sizes.
#[derive(Clone, Copy, Debug)]
pub struct Progress<'a> {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub entries_done: usize,
    pub entries_total: usize,
    /// Name of the entry being processed (empty on the final report).
    pub current: &'a str,
}

/// Receives progress and tells the core whether to stop. Called on the
/// thread doing the work, so keep it cheap. Reports are throttled by the
/// core: once per entry and about once per 256 KiB of data.
pub trait Observer {
    fn on_progress(&mut self, progress: &Progress<'_>);

    /// Checked before every entry and every written chunk.
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// An observer that ignores progress and never cancels.
pub struct NoObserver;

impl Observer for NoObserver {
    fn on_progress(&mut self, _progress: &Progress<'_>) {}
}

/// A shareable flag for cancelling from another thread; clone it into the
/// observer and keep a handle for the cancel button.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_is_shared_between_clones() {
        let token = CancelToken::new();
        let other = token.clone();
        assert!(!other.is_cancelled());
        token.cancel();
        assert!(other.is_cancelled());
    }
}
