use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

/// Cooperative cancellation signal shared across tasks working on the same
/// logical operation (e.g. a single transform pipeline).
///
/// Implementation note: we deliberately avoid pulling in `tokio-util` just for
/// `CancellationToken`. The combination of an `AtomicBool` with a `Notify`
/// gives us exactly the two behaviours we need:
/// - a synchronous `is_cancelled` probe for polling loops (so child-process
///   wait loops can bail out quickly without blocking on a future);
/// - an async `cancelled().await` for `tokio::select!` branches.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        // notify_waiters so every outstanding `cancelled().await` returns.
        self.inner.notify.notify_waiters();
    }

    /// Resolves once `cancel()` has been called. The double-check around
    /// `enable()` is what closes the race window between registering a
    /// listener and the cancellation flag being flipped.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
