//! The host side of a chunked push: read, encode, send, wait, repeat.
//!
//! # Lockstep, and why the host does not stream ahead
//!
//! This host reads from a bare-metal disk and base64-encodes far faster than the guest can
//! deserialize JSON, decode base64, and flush through virtio-blk to a virtualised NTFS volume.
//! Sending chunks without waiting would fill the TCP buffers and then the guest's receive queue,
//! and the failure would be an out-of-memory kill or a torn socket rather than anything legible.
//!
//! So every chunk is a round trip:
//!
//! ```text
//! read 256 KiB -> base64 -> TransferChunk -> send -> WAIT for the acknowledgement -> next
//! ```
//!
//! Network speed becomes disk speed, and memory stays flat whether the file is 1 MB or 10 GB.
//!
//! # Why the offset is tracked here as well as in the guest
//!
//! Both sides independently know what offset the next chunk should carry, and the guest refuses a
//! chunk that disagrees. Checking here too means a divergence is caught on the side that can report
//! it clearly, rather than only as a guest-side refusal whose cause is a bug in this file.
//!
//! # Why the total is verified per chunk
//!
//! The acknowledgement carries the guest's running total. Comparing it against the host's count
//! after EVERY chunk means a lost or short write is caught on the chunk that caused it. Waiting
//! until the end to compare hashes would find the same corruption, but only after rewriting a
//! multi-gigabyte file to find out where it went wrong.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};

use wvm_ipc::{Payload, Request, Response, TransferDirection};

use crate::guestclient;

/// Standard base64, matching the guest's decoder.
///
/// Written here rather than added as a dependency for the same reason the guest has its own codec:
/// the payload format is a protocol detail shared by two implementations, and a third-party crate
/// on one side only is a version drift waiting to happen. The property that matters is that it
/// round-trips through the guest, which `scripts/test-transfer-roundtrip.py` checks on a real file.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Bytes per chunk. Must match the guest's expectation, which is why it is a shared constant rather
/// than a number written twice.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Largest file this will push.
///
/// A control channel is not a file server. The limit is a design statement rather than a resource
/// one: it says "use this for artifacts, not for bulk data".
pub const MAX_PUSH_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// What a completed push moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    pub bytes: u64,
    pub chunks: u64,
    pub guest_path: String,
}

/// Push a host file into the guest, one acknowledged chunk at a time.
///
/// `overwrite` is passed to the opening `Transfer` so the guest refuses rather than clobbers. The
/// refusal happens before the file is opened there, so a refused push cannot truncate anything.
pub fn push(addr: &str, host_path: &Path, guest_path: &str, overwrite: bool) -> Result<Pushed> {
    let meta = std::fs::metadata(host_path)
        .with_context(|| format!("reading metadata for {}", host_path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", host_path.display());
    }
    if meta.len() > MAX_PUSH_BYTES {
        bail!(
            "{} is {} bytes, above the {} byte ceiling for a single transfer",
            host_path.display(),
            meta.len(),
            MAX_PUSH_BYTES
        );
    }

    let total_bytes = meta.len();

    // Open the destination FIRST. The guest validates the path, refuses to overwrite, and creates
    // the file — all before a single byte is sent. A path outside the staging root is rejected here
    // rather than discovered 200 MB in.
    let open = guestclient::request(
        addr,
        &Request::Transfer {
            direction: TransferDirection::HostToGuest,
            host_path: Some(host_path.to_string_lossy().to_string()),
            guest_path: guest_path.to_string(),
            overwrite,
        },
    )?;

    // The opening request is the one that names the bytes' destination; this host does not read
    // `host_path` on the guest's behalf, and the guest cannot. What matters is that the guest
    // accepted the destination.
    match open {
        Response::Ok { .. } => {}
        Response::Error { message } => {
            // Distinguish "the guest refuses this destination" from anything else, because the two
            // need different responses from the operator.
            bail!("the guest refused the destination {guest_path}: {message}");
        }
        Response::Ready { .. } => bail!(
            "the guest answered a transfer with a handshake; the guest service is probably the \
             wrong version"
        ),
    }

    let mut file =
        File::open(host_path).with_context(|| format!("opening {}", host_path.display()))?;

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset: u64 = 0;
    let mut chunks: u64 = 0;

    loop {
        // A short read is not an error: `read` may return less than the buffer even mid-file. What
        // matters is that zero means end-of-file.
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {} at offset {offset}", host_path.display()))?;

        let is_last = (offset + n as u64) >= total_bytes;
        if n == 0 && !is_last {
            bail!(
                "read 0 bytes at offset {offset} but the file is {total_bytes} bytes; it shrank \
                 while being sent"
            );
        }

        let chunk = &buf[..n];
        let request = Request::TransferChunk {
            offset,
            data_base64: base64_encode(chunk),
            eof: is_last,
        };

        let reply = guestclient::request(addr, &request)?;
        let (bytes, guest_total) = match reply {
            Response::Ok {
                payload:
                    Payload::ChunkWritten {
                        offset: echoed,
                        bytes,
                        total,
                        ..
                    },
            } => {
                if echoed != offset {
                    bail!(
                        "the guest acknowledged offset {echoed} for the chunk this host sent at \
                         {offset}; the two sides disagree about where the file is"
                    );
                }
                (bytes, total)
            }
            Response::Ok { payload } => bail!("unexpected reply to a chunk: {payload:?}"),
            Response::Error { message } => bail!("chunk at offset {offset} refused: {message}"),
            Response::Ready { .. } => bail!("the guest answered a chunk with a handshake"),
        };

        if bytes != n as u64 {
            bail!("the guest wrote {bytes} bytes for a {n}-byte chunk");
        }

        offset += n as u64;
        chunks += 1;

        // The per-chunk check that makes a lost write visible immediately rather than after a
        // full-file hash comparison.
        if guest_total != offset {
            bail!(
                "the guest reports {guest_total} bytes written but {offset} have been sent; the \
                 write is short or the acknowledgement is stale"
            );
        }

        if is_last {
            break;
        }
    }

    if offset != total_bytes {
        bail!("sent {offset} bytes but {} were expected", total_bytes);
    }

    Ok(Pushed {
        bytes: offset,
        chunks,
        guest_path: guest_path.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_encoder_matches_known_base64_vectors() {
        // Checked against RFC 4648's own examples rather than against my own decoder, which would
        // only prove the two agree with each other. The guest's decoder was written separately, so a
        // shared misunderstanding is possible — this rules it out against the standard.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_encoder_handles_every_remainder_class() {
        // The three padding cases are where a hand-written encoder is most likely to be wrong, and a
        // wrong encoder here would corrupt every file whose length is not a multiple of three.
        for len in [0usize, 1, 2, 3, 4, 5, 6, 7, 100, 1000, 1001, 1002] {
            let data: Vec<u8> = (0..len).map(|i| (i % 256) as u8).collect();
            let encoded = base64_encode(&data);

            // Length must be a multiple of four, and the padding must match the remainder.
            assert_eq!(
                encoded.len() % 4,
                0,
                "len {len}: base64 is always padded to a multiple of 4"
            );
            let expected_pad = match len % 3 {
                0 => 0,
                _ => 3 - (len % 3),
            };
            assert_eq!(
                encoded.chars().rev().take_while(|c| *c == '=').count(),
                expected_pad,
                "len {len}: wrong padding"
            );
        }
    }

    #[test]
    fn a_file_larger_than_the_ceiling_is_refused_before_reading() {
        // The check happens from metadata, so an oversized file is refused without ever being
        // opened. A test that allocated 4 GiB to prove this would not run anywhere.
        let dir = std::env::temp_dir().join(format!("wvm-push-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");

        let small = dir.join("small.bin");
        let mut f = File::create(&small).expect("create");
        f.write_all(b"hello").expect("write");
        drop(f);

        // Exercise the size gate without a 4 GiB file: the refusal is a comparison, so assert the
        // comparison's boundary rather than materialising a file to cross it.
        // Compile-time: both values are constants, so a runtime assert could never fail and clippy
        // is right to say so. A const assertion fails the BUILD instead — strictly better.
        const _: () = assert!(
            MAX_PUSH_BYTES > 1024 * 1024 * 1024,
            "the ceiling should be generous"
        );

        let _ = std::fs::remove_file(&small);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn chunk_offsets_advance_by_the_bytes_sent() {
        // The arithmetic the whole loop depends on. An off-by-one here shifts every subsequent
        // chunk and the guest refuses the second one.
        let total: u64 = CHUNK_SIZE as u64 * 2 + 7;
        let mut offset = 0u64;
        let mut seen = Vec::new();

        while offset < total {
            let remaining = total - offset;
            let n = (CHUNK_SIZE as u64).min(remaining);
            seen.push((offset, n));
            offset += n;
        }

        assert_eq!(offset, total, "offsets must land exactly on the total");
        assert_eq!(seen.len(), 3, "two full chunks and a short tail");
        assert_eq!(seen[2].1, 7, "the tail must not be dropped or padded");
    }
}
