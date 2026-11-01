//! An ordered mailbox: eight coalescing watcher slots plus a control lane.
//! Controls retain FIFO order with observations, particularly markers. Commands
//! retain a semaphore permit until their terminal reply; other controls are
//! produced only by the bounded set of jobs, readers and helpers.
use super::*;
use std::sync::{Mutex, atomic::AtomicBool};
use tokio::sync::Notify;

#[derive(Default)]
struct Slot {
    token: Token,
    delivered: bool,
    cancel: Option<Arc<AtomicBool>>,
}
#[derive(Default)]
struct Queue {
    messages: VecDeque<Message>,
    observations: VecDeque<Slot>,
    closed: bool,
}
#[derive(Default)]
struct Inbox {
    queue: Mutex<Queue>,
    ready: Notify,
}
#[derive(Clone)]
pub(in crate::core) struct Sender(Arc<Inbox>);
pub(in crate::core) struct Receiver(Arc<Inbox>);
pub(super) fn channel() -> (Sender, Receiver) {
    let inbox = Arc::new(Inbox::default());
    (Sender(inbox.clone()), Receiver(inbox))
}
fn observation_token(message: &Message) -> Option<Token> {
    match message {
        Message::Event(
            Event::Observe { token, .. }
            | Event::Read { token, .. }
            | Event::ImageTooLarge { token, .. },
        )
        | Message::ReadHandle(token, _) => Some(*token),
        _ => None,
    }
}
impl Sender {
    pub fn send(&self, message: Message) -> Result<(), ()> {
        let mut q = self.0.queue.lock().expect("clipboard mailbox");
        if q.closed {
            return Err(());
        }
        if let Message::Event(Event::Observe { token, .. }) = &message {
            if q.observations.len() == 8 {
                let old = q.observations.pop_front().unwrap();
                if let Some(cancel) = old.cancel {
                    cancel.store(true, Ordering::SeqCst);
                }
                q.messages
                    .retain(|m| observation_token(m) != Some(old.token));
                if old.delivered {
                    q.messages
                        .push_back(Message::Event(Event::AbandonRead(old.token)));
                }
            }
            q.observations.push_back(Slot {
                token: *token,
                ..Slot::default()
            });
        } else if let Some(token) = observation_token(&message) {
            let Some(slot) = q.observations.iter_mut().find(|s| s.token == token) else {
                if let Message::ReadHandle(_, cancel) = message {
                    cancel.store(true, Ordering::SeqCst);
                }
                return Ok(());
            };
            if let Message::ReadHandle(_, cancel) = &message {
                slot.cancel = Some(cancel.clone());
            }
            // At most one read result and one cancellation registration per slot.
            q.messages.retain(|m| !matches!((m, &message), (Message::Event(Event::ImageTooLarge { token: old, .. }), Message::Event(Event::ImageTooLarge { token: new, .. })) if old == new) && !matches!((m, &message),
                (Message::ReadHandle(old, _), Message::ReadHandle(new, _)) if old == new)
                && !matches!((m, &message), (Message::Event(Event::Read {token:old,..}), Message::Event(Event::Read {token:new,..})) if old == new));
        }
        q.messages.push_back(message);
        drop(q);
        self.0.ready.notify_one();
        Ok(())
    }
}
impl Receiver {
    #[cfg(all(test, unix))]
    pub fn len(&self) -> usize {
        self.0.queue.lock().unwrap().messages.len()
    }
    pub async fn recv(&mut self) -> Option<Message> {
        loop {
            let ready = self.0.ready.notified();
            {
                let mut q = self.0.queue.lock().expect("clipboard mailbox");
                if let Some(message) = q.messages.pop_front() {
                    if let Message::Event(Event::Observe { token, .. }) = &message
                        && let Some(slot) = q.observations.iter_mut().find(|s| s.token == *token)
                    {
                        slot.delivered = true;
                    }
                    return Some(message);
                }
                if q.closed {
                    return None;
                }
            }
            ready.await;
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        let mut q = self.0.queue.lock().expect("clipboard mailbox");
        q.closed = true;
        q.messages.clear();
        for slot in q.observations.drain(..) {
            if let Some(cancel) = slot.cancel {
                cancel.store(true, Ordering::SeqCst);
            }
        }
    }
}
