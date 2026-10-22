//! Blocking jobs continue after their awaiting future is canceled. Ownership
//! belongs to the closure; shutdown waits for its completion, including cleanup.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
// Bound submitted jobs, not their awaiting futures: cancellation must not hand
// capacity back while a detached blocking closure still owns its inputs.
pub(crate) const BLOCKING_LIMIT: usize = 16;
pub(crate) struct Work {
    active: AtomicUsize,
    changed: tokio::sync::Notify,
    slots: Arc<tokio::sync::Semaphore>,
}
impl Default for Work {
    fn default() -> Self {
        Self {
            active: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
            slots: Arc::new(tokio::sync::Semaphore::new(BLOCKING_LIMIT)),
        }
    }
}
pub(crate) struct Token(Vec<Arc<Work>>);
/// Capacity is secured before lifecycle changes or external effects. Ownership
/// can pass through a job's result to its durable continuation without a wait.
pub(crate) struct Reservation {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _work: Token,
}
impl Reservation {
    pub fn submit<T: Send + 'static>(
        self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        self.submit_with(move |_reservation| f())
    }
    pub fn submit_with<T: Send + 'static>(
        self,
        f: impl FnOnce(Self) -> T + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        tokio::task::spawn_blocking(move || f(self))
    }
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrackedWork")
    }
}
pub(crate) fn track(work: Vec<Arc<Work>>) -> Token {
    for jobs in &work {
        jobs.active.fetch_add(1, Ordering::SeqCst);
    }
    Token(work)
}
impl Drop for Token {
    fn drop(&mut self) {
        for work in &self.0 {
            work.active.fetch_sub(1, Ordering::SeqCst);
            work.changed.notify_waiters();
        }
    }
}
impl Work {
    pub async fn reserve(self: &Arc<Self>) -> Reservation {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("work slots");
        Reservation {
            _permit: permit,
            _work: track(vec![self.clone()]),
        }
    }
    #[cfg(test)]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
    #[cfg(test)]
    pub fn admitted(&self) -> usize {
        BLOCKING_LIMIT - self.slots.available_permits()
    }
    pub async fn drain(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            changed.await;
        }
    }
}
/// Fair admission also bounds repeated cancellations across sessions/actions.
/// Before admission cancellation drops the inputs; after admission the job owns
/// both its durability and its permit, including while queued in Tokio's pool.
pub(crate) async fn blocking<T: Send + 'static>(
    work: Arc<Work>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    work.reserve().await.submit(f).await
}

// Only cleanup and completion of already-admitted effects bypass admission.
// Their producers are bounded; these jobs cannot recursively produce more.
pub(crate) fn start_blocking<T: Send + 'static>(
    work: Vec<Arc<Work>>,
    f: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let token = track(work);
    tokio::task::spawn_blocking(move || {
        let result = f();
        drop(token);
        result
    })
}
