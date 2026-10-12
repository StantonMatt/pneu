//! Blocking jobs continue after their awaiting future is canceled. Ownership
//! belongs to the closure; shutdown waits for its completion, including cleanup.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Default)]
pub(crate) struct Work {
    active: AtomicUsize,
    changed: tokio::sync::Notify,
}
pub(crate) struct Token(Vec<Arc<Work>>);
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
    #[cfg(test)]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
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
