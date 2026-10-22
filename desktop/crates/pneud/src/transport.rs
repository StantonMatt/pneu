use anyhow::Result;
use pneu_proto::{Message, write_frame};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// A frame deadline includes TLS/buffer flushing, not just plaintext acceptance.
pub(crate) async fn send<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
    timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(timeout, async {
        write_frame(writer, message).await?;
        writer.flush().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
/// One overall fatal-close deadline covers frames, flushes and TLS close_notify.
pub(crate) async fn close<W: AsyncWrite + Unpin>(
    writer: &mut W,
    messages: &[Message],
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        for message in messages {
            write_frame(writer, message).await?;
            writer.flush().await?;
        }
        writer.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn buffered_frame_is_flushed_without_a_following_write() -> Result<()> {
        let (writer, mut reader) = tokio::io::duplex(3);
        let mut writer = tokio::io::BufWriter::new(writer);
        let msg = Message::Unpair(pneu_proto::Unpair {});
        let task =
            tokio::spawn(async move { send(&mut writer, &msg, Duration::from_secs(1)).await });
        assert!(matches!(
            pneu_proto::read_frame(&mut reader).await?,
            Some(Message::Unpair(_))
        ));
        task.await??;
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn backpressure_is_inside_send_and_close_deadlines() {
        let (writer, _reader) = tokio::io::duplex(1);
        let mut writer = tokio::io::BufWriter::new(writer);
        assert!(
            send(
                &mut writer,
                &Message::Unpair(pneu_proto::Unpair {}),
                Duration::from_millis(20)
            )
            .await
            .is_err()
        );
        // A separate stream: after a canceled frame the original is unusable.
        let (mut writer, _reader) = tokio::io::duplex(1);
        let start = tokio::time::Instant::now();
        assert!(
            close(&mut writer, &[Message::Unpair(pneu_proto::Unpair {})])
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}

/// Track actual outbound progress for the dialer's idle keepalive timer.
pub(crate) struct ActivityWriter<W> {
    pub writer: W,
    pub sent: std::sync::Arc<std::sync::Mutex<tokio::time::Instant>>,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for ActivityWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let result = std::pin::Pin::new(&mut self.writer).poll_write(cx, bytes);
        if matches!(result, std::task::Poll::Ready(Ok(n)) if n > 0) {
            *self.sent.lock().expect("write activity") = tokio::time::Instant::now();
        }
        result
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

pub(crate) type OutstandingPing =
    std::sync::Arc<std::sync::Mutex<Option<(pneu_proto::Nonce, tokio::time::Instant)>>>;
/// Separate ownership prevents application queue backpressure from suspending
/// either idle PING production or the outstanding PONG deadline.
pub(crate) async fn heartbeat(
    outbound: bool,
    activity: std::sync::Arc<std::sync::Mutex<tokio::time::Instant>>,
    priority: tokio::sync::mpsc::Sender<Message>,
    ping: OutstandingPing,
    close: tokio::sync::watch::Sender<Option<crate::core::CloseReason>>,
) {
    use tokio::time::{Instant, sleep_until, timeout_at};
    let mut stopped = close.subscribe();
    loop {
        let deadline = if outbound {
            *activity.lock().expect("write activity") + Duration::from_secs(60)
        } else {
            Instant::now() + Duration::from_secs(86400)
        };
        tokio::select! { biased;
            _=stopped.changed()=>return,
            _=sleep_until(deadline)=>{},
        }
        if !outbound {
            continue;
        }
        if *activity.lock().expect("write activity") + Duration::from_secs(60) > Instant::now() {
            continue;
        }
        let nonce = pneu_proto::Nonce::new(crate::random_hex(8)).expect("nonce");
        let deadline = Instant::now() + Duration::from_secs(30);
        *ping.lock().expect("heartbeat") = Some((nonce.clone(), deadline));
        let sent = tokio::select! {biased;
            _=stopped.changed()=>return,
            sent=timeout_at(deadline,priority.send(Message::Ping(pneu_proto::Ping {nonce})))=>sent,
        };
        if !matches!(sent, Ok(Ok(()))) {
            close.send_replace(Some(crate::core::CloseReason::Error(
                pneu_proto::ErrorCode::Timeout,
            )));
            return;
        }
        tokio::select! {biased;
            _=stopped.changed()=>return,
            _=sleep_until(deadline)=>{},
        }
        if ping.lock().expect("heartbeat").is_some() {
            close.send_replace(Some(crate::core::CloseReason::Error(
                pneu_proto::ErrorCode::Timeout,
            )));
            return;
        }
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::{
        sync::{mpsc, watch},
        time::{Instant, advance},
    };
    #[tokio::test(start_paused = true)]
    async fn pong_deadline_survives_saturated_application_and_priority_queues() {
        let activity = Arc::new(Mutex::new(Instant::now()));
        let (priority, mut wire) = mpsc::channel(1);
        let ping = Arc::new(Mutex::new(None));
        let (close, reason) = watch::channel(None);
        let task = tokio::spawn(heartbeat(
            true,
            activity,
            priority.clone(),
            ping.clone(),
            close,
        ));
        let (app, mut receiver) = mpsc::channel(1);
        app.send(Message::Unpair(pneu_proto::Unpair {}))
            .await
            .unwrap();
        let blocked =
            tokio::spawn(async move { app.send(Message::Unpair(pneu_proto::Unpair {})).await });
        advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(matches!(wire.recv().await, Some(Message::Ping(_))));
        priority
            .send(Message::Unpair(pneu_proto::Unpair {}))
            .await
            .unwrap();
        assert!(ping.lock().unwrap().is_some());
        advance(Duration::from_secs(30)).await;
        task.await.unwrap();
        assert!(matches!(
            *reason.borrow(),
            Some(crate::core::CloseReason::Error(
                pneu_proto::ErrorCode::Timeout
            ))
        ));
        assert!(!blocked.is_finished());
        receiver.close();
        blocked.await.unwrap().unwrap_err();
    }
    #[tokio::test(start_paused = true)]
    async fn saturated_priority_queue_cannot_prevent_timeout_before_ping_handoff() {
        let (priority, _wire) = mpsc::channel(1);
        priority
            .send(Message::Unpair(pneu_proto::Unpair {}))
            .await
            .unwrap();
        let (close, reason) = watch::channel(None);
        let task = tokio::spawn(heartbeat(
            true,
            Arc::new(Mutex::new(Instant::now())),
            priority,
            Arc::new(Mutex::new(None)),
            close,
        ));
        advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        advance(Duration::from_secs(30)).await;
        task.await.unwrap();
        assert!(matches!(
            *reason.borrow(),
            Some(crate::core::CloseReason::Error(
                pneu_proto::ErrorCode::Timeout
            ))
        ));
    }
}
