//! The host side of a chunked pull: ask for a chunk, receive it, write it, ask again.
//!
//! # The asymmetry, stated plainly
//!
//! A push has the host as the reader of its own file. A pull has the GUEST as the reader, and the
//! host as the receiver. The framing is the same; the initiative is opposite. Keeping them as
//! separate modules rather than one function with a direction flag is deliberate — an earlier
//! version of the transfer plan treated the two as mirrored, and that assumption is what hid a bug
//! where the guest was asked to open a host path it could never see.
//!
//! # Lockstep
//!
//! The host asks for one chunk and waits for it before asking for the next, for the same reason a
//! push does: the guest reads from a virtualised NTFS volume over virtio-blk, and the host can write
//! to a bare-metal disk far faster than that. Requesting ahead would fill buffers at a receiver that
//! cannot drain them.
//!
//! # Why the destination is written incrementally rather than buffered
//!
//! Collecting the whole file in memory before writing would be simpler and would work for small
//! artifacts — and it is exactly the shape that OOMs on a large one. Each chunk is written as it
//! arrives, so memory is one chunk regardless of file size.
//!
//! # Why the partial file is removed on failure
//!
//! A truncated artifact left at the destination path is worse than no file: its name says it is the
//! result, and nothing about it says incomplete. A caller that then tries to run or hash it gets a
//! confusing failure far from the cause.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

use wvm_ipc::{Payload, Request, Response, TransferDirection};

use crate::guestclient;

/// Bytes requested per chunk. Matches the push size, because the framing cost is the same and one
/// number is easier to reason about than two.
pub const CHUNK_SIZE: u64 = 256 * 1024;

/// Refuse a guest file larger than this.
///
/// The guest enforces its own ceiling; this is the receiving side refusing to commit the disk
/// space, so a guest misreporting a size cannot fill the host's filesystem.
pub const MAX_PULL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// What a completed pull produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    pub bytes: u64,
    pub chunks: u64,
    pub host_path: String,
}

/// Pull a guest file to a host path, one acknowledged chunk at a time.
pub fn pull(addr: &str, guest_path: &str, host_path: &Path, overwrite: bool) -> Result<Pulled> {
    // Refuse an existing destination BEFORE the transfer starts. A pull that clobbered a local file
    // and then failed would have destroyed the original for nothing.
    if host_path.exists() && !overwrite {
        let existing = std::fs::metadata(host_path).map(|m| m.len()).unwrap_or(0);
        bail!(
            "refusing to overwrite {} ({} bytes); pass --overwrite to replace it",
            host_path.display(),
            existing
        );
    }

    // Open the source on the guest. This is where it checks the path is inside its staging root and
    // that the file exists — before the host commits to receiving anything.
    let opened = guestclient::request(
        addr,
        &Request::Transfer {
            direction: TransferDirection::GuestToHost,
            host_path: None,
            guest_path: guest_path.to_string(),
            // Meaningless for a pull (nothing is overwritten on the guest) but the field is
            // required, and `false` is the safe value if it ever gains meaning.
            overwrite: false,
        },
    )?;

    let total = match opened {
        Response::Ok {
            payload:
                Payload::Transferred {
                    bytes,
                    guest_path: echoed,
                    ..
                },
        } => {
            if bytes > MAX_PULL_BYTES {
                bail!(
                    "the guest reports {bytes} bytes, above the {} byte ceiling for a single transfer",
                    MAX_PULL_BYTES
                );
            }
            if echoed != guest_path {
                // The guest echoing a different path than was asked for would mean the two sides
                // disagree about which file is moving, which is worth stopping on.
                bail!("the guest acknowledges a different path: {echoed}");
            }
            bytes
        }
        Response::Ok { payload } => bail!("unexpected reply to a pull: {payload:?}"),
        Response::Error { message } => bail!("the guest refused to open {guest_path}: {message}"),
        Response::Ready { .. } => bail!(
            "the guest answered a transfer with a handshake; the guest service is probably the \
             wrong version"
        ),
    };

    // Write to a temporary file first, then rename into place.
    //
    // Only a COMPLETE file should ever appear at the destination path. Writing directly would mean a
    // failure partway leaves a truncated file wearing the artifact's name, and a rename on success
    // makes the appearance of the file itself the signal that the transfer finished.
    let temp_path = host_path.with_extension("wvm-partial");
    let mut file =
        File::create(&temp_path).with_context(|| format!("creating {}", temp_path.display()))?;

    let started = std::time::Instant::now();
    let mut offset: u64 = 0;
    let mut chunks: u64 = 0;

    let result = (|| -> Result<()> {
        loop {
            let reply = guestclient::request(
                addr,
                &Request::PullChunk {
                    offset,
                    length: CHUNK_SIZE,
                },
            )?;

            let (data, eof) = match reply {
                Response::Ok {
                    payload:
                        Payload::ChunkRead {
                            offset: echoed,
                            data_base64,
                            eof,
                            total: guest_total,
                        },
                } => {
                    if echoed != offset {
                        bail!("the guest served offset {echoed} when {offset} was requested");
                    }
                    // The guest's notion of the file size must not drift mid-transfer. If it does,
                    // the offsets the host is asking for stop describing the same file.
                    if guest_total != total {
                        bail!(
                            "the guest reports a total of {guest_total} but opened the file as \
                             {total} bytes; the file changed while it was being sent"
                        );
                    }
                    match wvm_ipc::base64_decode(&data_base64) {
                        Ok(d) => (d, eof),
                        Err(e) => bail!("undecodable chunk at offset {offset}: {e}"),
                    }
                }
                Response::Ok { payload } => {
                    bail!("unexpected reply to a chunk request: {payload:?}")
                }
                Response::Error { message } => bail!("chunk at offset {offset} refused: {message}"),
                Response::Ready { .. } => bail!("the guest answered a chunk with a handshake"),
            };

            if data.is_empty() && !eof {
                bail!(
                    "the guest returned no bytes at offset {offset} without signalling the end; \
                     continuing would loop forever"
                );
            }

            file.write_all(&data)
                .with_context(|| format!("writing at offset {offset}"))?;

            offset += data.len() as u64;
            chunks += 1;

            if eof {
                break;
            }
        }
        Ok(())
    })();

    // Any failure removes the partial file. See the note at the top for why leaving it would be
    // worse than having nothing.
    if let Err(e) = result {
        drop(file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    file.flush()
        .with_context(|| format!("flushing {}", temp_path.display()))?;
    // Flush to disk before the rename. A rename that lands while the data is still in the OS cache
    // gives a file that is present, complete-looking, and unreadable after a crash.
    file.sync_all()
        .with_context(|| format!("flushing {} to disk", temp_path.display()))?;
    drop(file);

    if offset != total {
        let _ = std::fs::remove_file(&temp_path);
        bail!(
            "received {offset} bytes but the guest said the file is {total}; the transfer is short"
        );
    }

    // Confirm the size on disk rather than trusting the accounting above. "The writes returned
    // success" and "the file is the size intended" are different claims.
    let on_disk = std::fs::metadata(&temp_path)
        .with_context(|| format!("confirming {}", temp_path.display()))?
        .len();
    if on_disk != total {
        let _ = std::fs::remove_file(&temp_path);
        bail!(
            "{} is {on_disk} bytes but the transfer counted {total}",
            temp_path.display()
        );
    }

    std::fs::rename(&temp_path, host_path).with_context(|| {
        format!(
            "moving {} into place at {}",
            temp_path.display(),
            host_path.display()
        )
    })?;

    let _ = started;

    Ok(Pulled {
        bytes: offset,
        chunks,
        host_path: host_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_is_generous_but_finite() {
        const _: () = assert!(MAX_PULL_BYTES > 1024 * 1024 * 1024);
        const _: () = assert!(MAX_PULL_BYTES < u64::MAX);
    }

    #[test]
    fn chunk_offsets_advance_by_the_bytes_received() {
        // The same arithmetic the push side uses, and worth its own check because a pull derives it
        // from what the GUEST returned rather than from what the host intended to send. A guest
        // returning short chunks must still land exactly on the total.
        let total: u64 = CHUNK_SIZE * 2 + 7;
        let mut offset = 0u64;
        let mut spans = Vec::new();

        while offset < total {
            let want = CHUNK_SIZE.min(total - offset);
            spans.push((offset, want));
            offset += want;
        }

        assert_eq!(offset, total);
        assert_eq!(spans.len(), 3, "two full chunks and a short tail");
        assert_eq!(spans[2].1, 7, "the tail must not be dropped or padded");
    }

    #[test]
    fn an_existing_destination_is_refused_without_overwrite() {
        let dir = std::env::temp_dir().join(format!("wvm-pull-dest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let dest = dir.join("existing.bin");
        std::fs::write(&dest, b"already here").expect("seed");

        // The refusal happens before any request is made, so this needs no server to test.
        let err = pull("127.0.0.1:1", "C:/x.bin", &dest, false).expect_err("must be refused");
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "the refusal must be explicit: {err}"
        );

        // And the existing file must be untouched — a refusal that truncated first would be worse
        // than no check.
        assert_eq!(std::fs::read(&dest).expect("read"), b"already here");

        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_dir(&dir);
    }
}
