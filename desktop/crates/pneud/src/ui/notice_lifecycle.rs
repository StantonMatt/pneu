//! One request's state, shared by submission, cancellation and expiry callbacks.
use std::{sync::Mutex, time::Duration};

#[cfg(any(target_os = "macos", test))]
pub(crate) fn stale_identifiers(
    identifiers: Vec<String>,
    active: impl Fn(&str) -> bool,
) -> Vec<String> {
    identifiers
        .into_iter()
        .filter(|id| id.starts_with("pneu-") && !active(id))
        .collect()
}

/// Categories are projected from live records, including persistent notices.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn live_values<'a, T: 'a>(records: impl Iterator<Item = (&'a Lifecycle, T)>) -> Vec<T> {
    records
        .filter_map(|(life, value)| life.active().then_some(value))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Issued,
    Acknowledged,
    Cancelled,
    Expired,
}
pub(crate) struct Lifecycle {
    phase: Mutex<Phase>,
    pub deadline: Option<tokio::time::Instant>,
}
impl Lifecycle {
    pub fn new(lifetime: Option<Duration>) -> Self {
        Self {
            phase: Mutex::new(Phase::Issued),
            deadline: lifetime.map(|d| tokio::time::Instant::now() + d),
        }
    }
    fn current(&self, phase: &mut Phase) -> bool {
        if self
            .deadline
            .is_some_and(|d| tokio::time::Instant::now() >= d)
            && matches!(phase, Phase::Issued | Phase::Acknowledged)
        {
            *phase = Phase::Expired;
        }
        matches!(phase, Phase::Issued | Phase::Acknowledged)
    }
    pub fn active(&self) -> bool {
        self.current(&mut self.phase.lock().expect("notification lifecycle"))
    }
    /// Serialize the native submit boundary with cancellation. The callback may
    /// retain this record, but can never resurrect a cancelled/expired request.
    pub fn submit(&self, post: impl FnOnce()) -> bool {
        let mut phase = self.phase.lock().expect("notification lifecycle");
        if !self.current(&mut phase) {
            return false;
        }
        post();
        true
    }
    /// Check at the actual OS boundary, serialized with channel cancellation.
    /// Do not re-enter this lifecycle from `operation` (native callbacks defer).
    pub fn run<T>(&self, operation: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut result = None;
        anyhow::ensure!(
            self.submit(|| result = Some(operation())),
            "UI request cancelled or expired"
        );
        result.expect("active operation ran")
    }
    pub fn acknowledge(&self) -> bool {
        let mut phase = self.phase.lock().expect("notification lifecycle");
        if !self.current(&mut phase) {
            return false;
        }
        *phase = Phase::Acknowledged;
        true
    }
    pub fn cancel(&self) {
        let mut phase = self.phase.lock().expect("notification lifecycle");
        if self.current(&mut phase) {
            *phase = Phase::Cancelled;
        }
    }
}

#[cfg(any(target_os = "macos", test))]
type Completion = Box<dyn FnOnce(anyhow::Result<()>) + Send + 'static>;

/// Local picker/Recent futures revoke queued native work synchronously on drop.
#[cfg(any(target_os = "macos", test))]
pub(crate) struct CancelOnDrop(pub std::sync::Arc<Lifecycle>);
#[cfg(any(target_os = "macos", test))]
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// A deferred native request always completes, even when dispatch/submission is
/// rejected. Clones share one completion; the last dropped clone fails closed.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone)]
pub(crate) struct RequestGuard(std::sync::Arc<GuardInner>);
#[cfg(any(target_os = "macos", test))]
struct GuardInner {
    life: std::sync::Arc<Lifecycle>,
    completion: Mutex<Option<Completion>>,
}
#[cfg(any(target_os = "macos", test))]
impl RequestGuard {
    pub fn new(
        life: std::sync::Arc<Lifecycle>,
        completion: impl FnOnce(anyhow::Result<()>) + Send + 'static,
    ) -> Self {
        Self(std::sync::Arc::new(GuardInner {
            life,
            completion: Mutex::new(Some(Box::new(completion))),
        }))
    }
    pub fn life(&self) -> &std::sync::Arc<Lifecycle> {
        &self.0.life
    }
    pub fn run<T>(&self, operation: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.0.life.run(operation)
    }
    pub fn submit(&self, operation: impl FnOnce()) -> bool {
        let accepted = self.0.life.submit(operation);
        if !accepted {
            self.finish(Err(anyhow::anyhow!("UI request cancelled or expired")));
        }
        accepted
    }
    pub fn finish(&self, result: anyhow::Result<()>) {
        let completion = self.0.completion.lock().expect("UI completion").take();
        if let Some(completion) = completion {
            completion(result);
        }
    }
}
#[cfg(any(target_os = "macos", test))]
impl Drop for GuardInner {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.get_mut().expect("UI completion").take() {
            completion(Err(anyhow::anyhow!("UI request ended before completion")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    fn request(life: Arc<Lifecycle>) -> (RequestGuard, mpsc::Receiver<bool>) {
        let (send, receive) = mpsc::channel();
        (
            RequestGuard::new(life, move |r| send.send(r.is_ok()).unwrap()),
            receive,
        )
    }
    #[test]
    fn cancelled_queued_and_prepared_handoffs_complete_once_without_opening() {
        for prepared in [false, true] {
            let life = Arc::new(Lifecycle::new(None));
            let (guard, replies) = request(life.clone());
            if prepared {
                assert!(guard.life().active());
            }
            life.cancel(); // channel thread; main-thread cleanup has not run
            guard.finish(guard.run(|| panic!("revoked viewer reached OS")));
            guard.finish(Ok(())); // late native/cancellation callback
            drop(guard);
            assert_eq!(replies.try_iter().collect::<Vec<_>>(), vec![false]);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn rejected_submission_after_active_check_acknowledges_cancel_and_expiry() {
        for expires in [false, true] {
            let life = Arc::new(Lifecycle::new(Some(Duration::from_secs(1))));
            let (guard, replies) = request(life.clone());
            assert!(life.active());
            if expires {
                tokio::time::advance(Duration::from_secs(1)).await;
            } else {
                life.cancel();
            }
            assert!(!guard.submit(|| panic!("rejected notification posted")));
            // Rejection completes immediately, before dropping the callback.
            assert!(!replies.try_recv().unwrap());
            guard.finish(Ok(()));
            drop(guard);
            assert!(replies.try_iter().next().is_none());
        }
    }
    #[test]
    fn abandoned_dispatch_and_native_errors_complete_exactly_once() {
        let (guard, replies) = request(Arc::new(Lifecycle::new(None)));
        let callback = guard.clone();
        drop(guard);
        assert!(replies.try_recv().is_err());
        drop(callback); // e.g. invoke_from_event_loop rejected the closure
        assert_eq!(replies.try_iter().collect::<Vec<_>>(), vec![false]);
        let (guard, replies) = request(Arc::new(Lifecycle::new(None)));
        guard.finish(guard.run(|| Err(anyhow::anyhow!("OS rejected request"))));
        guard.finish(Ok(()));
        drop(guard);
        assert_eq!(replies.try_iter().collect::<Vec<_>>(), vec![false]);
    }
    #[test]
    fn cancellation_waits_for_an_already_started_os_handoff() {
        let life = Arc::new(Lifecycle::new(None));
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let operation_life = life.clone();
        let operation = std::thread::spawn(move || {
            operation_life.run(|| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
        });
        entering.recv().unwrap();
        let cancel_life = life.clone();
        let cancellation = std::thread::spawn(move || cancel_life.cancel());
        release.send(()).unwrap();
        operation.join().unwrap().unwrap();
        cancellation.join().unwrap();
        assert!(life.run::<()>(|| panic!("post-cancel handoff")).is_err());
    }
    #[test]
    fn local_future_drop_revokes_deferred_picker_or_recent_open() {
        let life = Arc::new(Lifecycle::new(None));
        let cancel = CancelOnDrop(life.clone());
        drop(cancel);
        assert!(
            life.run::<()>(|| panic!("dropped local request reached OS"))
                .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn persistent_category_survives_more_than_128_expired_transients() {
        let persistent = Lifecycle::new(None);
        let mut records = vec![persistent];
        for _ in 0..140 {
            records.push(Lifecycle::new(Some(Duration::from_secs(1))));
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            live_values(records.iter().enumerate().map(|(id, life)| (life, id))),
            vec![0]
        );
        records[0].cancel();
        assert!(live_values(records.iter().map(|life| (life, ()))).is_empty());
    }
    #[test]
    fn replacement_child_sweep_removes_previous_daemon_namespaces_and_preserves_live() {
        let ids = [
            "pneu-session-old-1",
            "pneu-session-new-1",
            "pneu-other-old-2",
            "unrelated-notification",
        ];
        assert_eq!(
            stale_identifiers(ids.map(str::to_owned).into(), |id| id == ids[1]),
            vec![ids[0], ids[2]]
        );
    }
    #[test]
    fn cancellation_before_deferred_settings_prevents_posting() {
        let life = Lifecycle::new(None);
        life.cancel();
        assert!(!life.submit(|| panic!("cancelled request posted")));
        assert!(!life.acknowledge());
    }
    #[tokio::test(start_paused = true)]
    async fn expiry_is_terminal_before_or_after_acknowledgement() {
        for ack in [false, true] {
            let life = Lifecycle::new(Some(Duration::from_secs(4)));
            if ack {
                assert!(life.acknowledge());
            }
            tokio::time::advance(Duration::from_secs(4)).await;
            assert!(!life.active());
            assert!(!life.submit(|| panic!("expired request posted")));
            life.cancel();
            assert!(!life.acknowledge());
        }
    }
}
