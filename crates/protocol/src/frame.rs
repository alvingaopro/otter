//! Binary frames used in attach mode.
//!
//! ```text
//! +------+-------------+-----------------+
//! | kind | len (u32 BE)| payload (len B) |
//! +------+-------------+-----------------+
//! ```

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const KIND_DATA: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_DETACH: u8 = 3;
const KIND_EXIT: u8 = 4;

/// Maximum payload accepted from the peer.
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Terminal bytes. Client→daemon: input. Daemon→client: output.
    Data(Vec<u8>),
    /// Client→daemon: the client terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Client→daemon: detach (the session keeps running).
    Detach,
    /// Daemon→client: the attach ended. Always the last frame.
    Exit(AttachExit),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachExit {
    pub reason: AttachExitReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachExitReason {
    /// The client asked to detach.
    Detached,
    /// The session's process exited.
    Exited,
    /// The session was stopped, deleted or lost.
    Ended,
    /// Something went wrong in the attach bridge.
    Error,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> std::io::Result<()> {
    let (kind, payload): (u8, std::borrow::Cow<'_, [u8]>) = match frame {
        Frame::Data(d) => (KIND_DATA, d.as_slice().into()),
        Frame::Resize { cols, rows } => {
            let mut p = Vec::with_capacity(4);
            p.extend_from_slice(&cols.to_be_bytes());
            p.extend_from_slice(&rows.to_be_bytes());
            (KIND_RESIZE, p.into())
        }
        Frame::Detach => (KIND_DETACH, Vec::new().into()),
        Frame::Exit(exit) => (
            KIND_EXIT,
            serde_json::to_vec(exit)
                .expect("AttachExit serializes")
                .into(),
        ),
    };
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_LEN)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&len.to_be_bytes());
    w.write_all(&header).await?;
    w.write_all(&payload).await?;
    w.flush().await
}

/// Read one frame. Returns `Ok(None)` on a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(header[1..].try_into().unwrap());
    if len > MAX_FRAME_LEN {
        return Err(invalid(format!("frame of {len} bytes exceeds limit")));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    let frame = match header[0] {
        KIND_DATA => Frame::Data(payload),
        KIND_RESIZE => {
            if payload.len() != 4 {
                return Err(invalid("bad resize frame".into()));
            }
            Frame::Resize {
                cols: u16::from_be_bytes([payload[0], payload[1]]),
                rows: u16::from_be_bytes([payload[2], payload[3]]),
            }
        }
        KIND_DETACH => Frame::Detach,
        KIND_EXIT => Frame::Exit(
            serde_json::from_slice(&payload)
                .map_err(|e| invalid(format!("bad exit frame: {e}")))?,
        ),
        other => return Err(invalid(format!("unknown frame kind {other}"))),
    };
    Ok(Some(frame))
}

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let frames = vec![
            Frame::Data(b"hello\r\n".to_vec()),
            Frame::Resize {
                cols: 120,
                rows: 40,
            },
            Frame::Detach,
            Frame::Exit(AttachExit {
                reason: AttachExitReason::Exited,
                exit_code: Some(2),
                message: None,
            }),
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).await.unwrap();
        }
        let mut cursor = std::io::Cursor::new(buf);
        for f in &frames {
            assert_eq!(read_frame(&mut cursor).await.unwrap().as_ref(), Some(f));
        }
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }
}
