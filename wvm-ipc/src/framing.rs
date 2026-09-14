//! Length-prefixed framing for the WVM control channel.
//!
//! Wire format, per message:
//!
//! ```text
//! +--------+--------+--------+--------+----------------------+
//! | len_be (4 bytes, u32)              | payload (len bytes)  |
//! +--------+--------+--------+--------+----------------------+
//! ```
//!
//! `len` counts the payload only and excludes the header. The maximum is
//! [`MAX_FRAME_LEN`]; anything larger is refused before allocation, so a hostile or buggy peer
//! cannot make the process reserve arbitrary memory.
//!
//! This module is deliberately transport-agnostic: it works over any `Read`/`Write`, which is
//! what lets TCP be the default while `vsock` remains a drop-in alternative (see
//! `docs/DECISIONS.md` D-002).

use std::io::{self, Read, Write};

/// Largest payload accepted, in bytes. 64 MiB is comfortably above a full-screen PNG frame and
/// far below anything that would be reasonable to buffer per message.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    /// The peer closed the connection cleanly at a message boundary.
    Closed,
    /// The peer closed mid-message, or the payload was shorter than its header claimed.
    Truncated { expected: u32, got: usize },
    /// The header declared a payload larger than [`MAX_FRAME_LEN`].
    TooLarge { declared: u32, max: u32 },
    /// Underlying transport failure.
    Io(io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Closed => write!(f, "connection closed"),
            FrameError::Truncated { expected, got } => {
                write!(
                    f,
                    "truncated frame: header said {expected} bytes, read {got}"
                )
            }
            FrameError::TooLarge { declared, max } => {
                write!(f, "frame too large: {declared} bytes exceeds maximum {max}")
            }
            FrameError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Write one length-prefixed message.
///
/// Returns [`FrameError::TooLarge`] rather than writing a partial frame, so a caller that
/// exceeds the limit cannot leave the peer reading a header it will never satisfy.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> Result<(), FrameError> {
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        declared: u32::MAX,
        max: MAX_FRAME_LEN,
    })?;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            declared: len,
            max: MAX_FRAME_LEN,
        });
    }
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed message.
///
/// A clean EOF *before* the header is [`FrameError::Closed`] (normal shutdown); an EOF partway
/// through is [`FrameError::Truncated`] (a fault worth reporting). The distinction matters
/// because treating a truncated frame as a clean close hides real bugs.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];

    // Read the header, distinguishing clean close from mid-header truncation.
    let mut filled = 0;
    while filled < 4 {
        match r.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Err(FrameError::Closed),
            Ok(0) => {
                return Err(FrameError::Truncated {
                    expected: 4,
                    got: filled,
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }

    let len = u32::from_be_bytes(header);
    if len > MAX_FRAME_LEN {
        // Refuse before allocating. This is the hostile-peer guard.
        return Err(FrameError::TooLarge {
            declared: len,
            max: MAX_FRAME_LEN,
        });
    }

    let mut payload = vec![0u8; len as usize];
    let mut got = 0usize;
    while got < payload.len() {
        match r.read(&mut payload[got..]) {
            Ok(0) => {
                return Err(FrameError::Truncated { expected: len, got });
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_empty() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_frame(&mut c).unwrap(), b"");
    }

    #[test]
    fn round_trip_payload() {
        let msg = br#"{"op":"inspect"}"#;
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_frame(&mut c).unwrap(), msg.as_slice());
    }

    #[test]
    fn sequential_frames_stay_aligned() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"one").unwrap();
        write_frame(&mut buf, b"two").unwrap();
        write_frame(&mut buf, b"three").unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_frame(&mut c).unwrap(), b"one");
        assert_eq!(read_frame(&mut c).unwrap(), b"two");
        assert_eq!(read_frame(&mut c).unwrap(), b"three");
    }

    #[test]
    fn clean_eof_at_boundary_is_closed_not_truncated() {
        let mut c = Cursor::new(Vec::new());
        match read_frame(&mut c) {
            Err(FrameError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn partial_header_is_truncated() {
        // Header claims 8 bytes but only 2 arrive.
        let mut c = Cursor::new(vec![0, 0, 0, 8, 0xAA, 0xBB]);
        match read_frame(&mut c) {
            Err(FrameError::Truncated { expected, got }) => {
                assert_eq!(expected, 8);
                assert_eq!(got, 2);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_header_is_truncated() {
        // Only two of four header bytes.
        let mut c = Cursor::new(vec![0, 0]);
        match read_frame(&mut c) {
            Err(FrameError::Truncated { expected, got }) => {
                assert_eq!(expected, 4);
                assert_eq!(got, 2);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn oversized_frame_refused_before_allocating() {
        // Declare u32::MAX; must be refused, not attempted.
        let mut c = Cursor::new(u32::MAX.to_be_bytes().to_vec());
        match read_frame(&mut c) {
            Err(FrameError::TooLarge { declared, max }) => {
                assert_eq!(declared, u32::MAX);
                assert_eq!(max, MAX_FRAME_LEN);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn writing_oversized_payload_is_refused() {
        let big = vec![0u8; (MAX_FRAME_LEN as usize) + 1];
        let mut buf = Vec::new();
        match write_frame(&mut buf, &big) {
            Err(FrameError::TooLarge { .. }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn max_size_payload_is_accepted() {
        let exact = vec![7u8; MAX_FRAME_LEN as usize];
        let mut buf = Vec::new();
        write_frame(&mut buf, &exact).unwrap();
        let mut c = Cursor::new(buf);
        let out = read_frame(&mut c).unwrap();
        assert_eq!(out.len(), MAX_FRAME_LEN as usize);
        assert!(out.iter().all(|&b| b == 7));
    }

    #[test]
    fn header_is_big_endian() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"ab").unwrap();
        assert_eq!(&buf[..4], &[0, 0, 0, 2]);
        assert_eq!(&buf[4..], b"ab");
    }
}
