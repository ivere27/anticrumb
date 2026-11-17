use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    mutex: Mutex<()>,
    condvar: Condvar,
    notify: tokio::sync::Notify,
}

/// Cheap, clonable cooperative-cancellation flag shared between an agent run, its tools and
/// whoever supervises it (job manager, Ctrl-C handler, ...).
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        if let Ok(_guard) = self.0.mutex.lock() {
            self.0.condvar.notify_all();
        }
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// `Err("cancelled")` once the token has been triggered.
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("cancelled".to_string())
        } else {
            Ok(())
        }
    }

    /// Resolves immediately once the token has been cancelled.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Sleep up to `total`, waking early (returning `true`) as soon as the token is cancelled.
    pub fn sleep_or_cancelled(&self, total: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let deadline = Instant::now() + total;
        let mut guard = self.0.mutex.lock().unwrap_or_else(|e| e.into_inner());
        while !self.is_cancelled() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, timeout) = self
                .0
                .condvar
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if timeout.timed_out() {
                break;
            }
        }
        self.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_state() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(a.check().is_ok());
        b.cancel();
        assert!(a.is_cancelled());
        assert_eq!(a.check().unwrap_err(), "cancelled");
        assert!(a.sleep_or_cancelled(Duration::from_secs(5)));
    }
}
