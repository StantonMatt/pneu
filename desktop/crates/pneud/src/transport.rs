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
    #[tokio::test]
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
