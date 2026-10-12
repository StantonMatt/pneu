use crate::{FileChunk, Id, MAX_CONTROL_PAYLOAD, MAX_FRAME_SIZE, Message, ProtocolError, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn check_length(length: usize) -> Result<()> {
    if length == 0 || length > MAX_FRAME_SIZE {
        Err(ProtocolError::InvalidLength(length))
    } else {
        Ok(())
    }
}

fn check_type_and_size(type_id: u8, payload_len: usize) -> Result<()> {
    match type_id {
        0x11 => {
            if !(25..=24 + crate::CHUNK_SIZE).contains(&payload_len) {
                return Err(ProtocolError::InvalidLength(payload_len + 1));
            }
        }
        0x01..=0x07 | 0x10 | 0x12..=0x14 | 0x20 | 0x7f => {
            if payload_len > MAX_CONTROL_PAYLOAD {
                return Err(ProtocolError::InvalidLength(payload_len + 1));
            }
        }
        other => return Err(ProtocolError::UnknownType(other)),
    }
    Ok(())
}

fn decode_payload(type_id: u8, payload: &[u8]) -> Result<Message> {
    check_type_and_size(type_id, payload.len())?;
    if type_id != 0x11 {
        std::str::from_utf8(payload)
            .map_err(|_| ProtocolError::InvalidValue("control payload is not UTF-8".into()))?;
        crate::require(
            payload.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{'),
            "control payload must be an object",
        )?;
    }
    let message = match type_id {
        0x01 => Message::Hello(serde_json::from_slice(payload)?),
        0x02 => Message::PairRequest(serde_json::from_slice(payload)?),
        0x03 => Message::PairResult(serde_json::from_slice(payload)?),
        0x04 => Message::Ping(serde_json::from_slice(payload)?),
        0x05 => Message::Pong(serde_json::from_slice(payload)?),
        0x06 => Message::Status(serde_json::from_slice(payload)?),
        0x07 => Message::Unpair(serde_json::from_slice(payload)?),
        0x10 => Message::FileBegin(serde_json::from_slice(payload)?),
        0x11 => {
            let id = Id::from_bytes(payload[..16].try_into().expect("checked chunk size"));
            let offset =
                u64::from_be_bytes(payload[16..24].try_into().expect("checked chunk size"));
            Message::FileChunk(FileChunk {
                transfer_id: id,
                offset,
                data: payload[24..].to_vec(),
            })
        }
        0x12 => Message::FileEnd(serde_json::from_slice(payload)?),
        0x13 => Message::Ack(serde_json::from_slice(payload)?),
        0x14 => Message::Nack(serde_json::from_slice(payload)?),
        0x20 => Message::Clipboard(serde_json::from_slice(payload)?),
        0x7f => Message::Error(serde_json::from_slice(payload)?),
        other => return Err(ProtocolError::UnknownType(other)),
    };
    message.validate()?;
    Ok(message)
}

/// Decode exactly one complete frame (rejects trailing bytes, not a stream parser).
pub fn decode_frame(bytes: &[u8]) -> Result<Message> {
    if bytes.len() < 4 {
        return Err(ProtocolError::InvalidLength(bytes.len()));
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().expect("checked prefix")) as usize;
    check_length(length)?;
    if bytes.len() != length + 4 {
        return Err(ProtocolError::InvalidLength(bytes.len()));
    }
    decode_payload(bytes[4], &bytes[5..])
}

/// Encode one frame. Control JSON serialization order is not a wire guarantee.
pub fn encode_frame(message: &Message) -> Result<Vec<u8>> {
    message.validate()?;
    let payload = match message {
        Message::FileChunk(chunk) => {
            // One allocation/copy for bulk bytes instead of a payload buffer
            // followed by a second full-frame buffer.
            let length = 25 + chunk.data.len();
            let mut frame = Vec::with_capacity(length + 4);
            frame.extend_from_slice(&(length as u32).to_be_bytes());
            frame.push(0x11);
            frame.extend_from_slice(&chunk.transfer_id.to_bytes());
            frame.extend_from_slice(&chunk.offset.to_be_bytes());
            frame.extend_from_slice(&chunk.data);
            return Ok(frame);
        }
        Message::Hello(m) => serde_json::to_vec(m)?,
        Message::Status(m) => serde_json::to_vec(m)?,
        Message::Unpair(m) => serde_json::to_vec(m)?,
        Message::PairRequest(m) => serde_json::to_vec(m)?,
        Message::PairResult(m) => serde_json::to_vec(m)?,
        Message::Ping(m) | Message::Pong(m) => serde_json::to_vec(m)?,
        Message::FileBegin(m) => serde_json::to_vec(m)?,
        Message::FileEnd(m) => serde_json::to_vec(m)?,
        Message::Ack(m) => serde_json::to_vec(m)?,
        Message::Nack(m) => serde_json::to_vec(m)?,
        Message::Clipboard(m) => serde_json::to_vec(m)?,
        Message::Error(m) => serde_json::to_vec(m)?,
    };
    let type_id = message.type_id();
    check_type_and_size(type_id, payload.len())?;
    let length = payload.len() + 1;
    check_length(length)?;
    let mut frame = Vec::with_capacity(length + 4);
    frame.extend_from_slice(&(length as u32).to_be_bytes());
    frame.push(type_id);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Read one frame. `None` means clean EOF at a frame boundary; a partial frame is
/// an error. Limits/type are checked before allocating the payload. Not cancel-safe:
/// if canceled after consuming bytes, the caller MUST close this connection.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Message>> {
    let mut prefix = [0; 4];
    if reader.read(&mut prefix[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut prefix[1..]).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    check_length(length)?;
    let type_id = reader.read_u8().await?;
    check_type_and_size(type_id, length - 1)?;
    if type_id == 0x11 {
        // Read bulk data directly into its owned buffer; no second chunk copy.
        let mut header = [0; 24];
        reader.read_exact(&mut header).await?;
        let mut data = vec![0; length - 25];
        reader.read_exact(&mut data).await?;
        let message = Message::FileChunk(FileChunk {
            transfer_id: Id::from_bytes(header[..16].try_into().expect("fixed header")),
            offset: u64::from_be_bytes(header[16..].try_into().expect("fixed header")),
            data,
        });
        message.validate()?;
        return Ok(Some(message));
    }
    let mut payload = vec![0; length - 1];
    reader.read_exact(&mut payload).await?;
    decode_payload(type_id, &payload).map(Some)
}

/// Write one complete frame; caller owns writer serialization and must flush a
/// buffered writer promptly. Not cancel-safe: close the stream on cancellation.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    if let Message::FileChunk(chunk) = message {
        message.validate()?;
        let mut header = [0u8; 29];
        header[..4].copy_from_slice(&((25 + chunk.data.len()) as u32).to_be_bytes());
        header[4] = 0x11;
        header[5..21].copy_from_slice(&chunk.transfer_id.to_bytes());
        header[21..].copy_from_slice(&chunk.offset.to_be_bytes());
        writer.write_all(&header).await?;
        writer.write_all(&chunk.data).await?;
    } else {
        writer.write_all(&encode_frame(message)?).await?;
    }
    Ok(())
}
