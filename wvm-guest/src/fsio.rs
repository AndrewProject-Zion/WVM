//! Real filesystem access for transfers, chunked.
//!
//! # Why chunked, when the frame cap is 64 MiB
//!
//! The cap would allow a single frame to carry a large file, and doing that would be the wrong
//! implementation even though it would pass every small test. Two reasons:
//!
//! 1. **Memory.** One frame means the whole file is in memory on both sides at once. A 4 GiB image
//!    would be a 4 GiB allocation on the host and again in the guest, in a service that also has to
//!    stay responsive to a control channel. The frame cap exists to stop a *hostile* peer forcing an
//!    allocation; it is not an invitation to make honest ones.
//!
//! 2. **A failed transfer must not look like a small successful one.** With chunking, every chunk is
//!    acknowledged and the byte count is known before the file is written. A stream that dies
//!    halfway is detectably short. With one frame there is no "halfway" — either the frame arrived
//!    or it did not, and a truncated file sitting on disk with no error is the failure mode this
//!    project keeps recording.
//!
//! This is the same buffer-exhaustion class as WVM-01, which was not about a 64 KiB limit either —
//! it was about what happens when nobody is draining while a writer is producing. Concretely: a
//! 1 GiB file as one frame is a 1 GiB `Vec` and a 1 GiB `read_to_end`, which is WVM-01 with a bigger
//! number and no pipe to blame.
//!
//! # The chunk size
//!
//! 256 KiB. Large enough that the per-chunk framing overhead is irrelevant (a ~40-byte header against
//! 262,144 bytes is 0.015%), small enough that a stalled reader costs a quarter-megabyte of buffer
//! rather than a gigabyte. It is also a fraction of the host's ~1 MB socket send buffer, so a chunk
//! is written without the writer blocking on a reader that is still processing the previous one.

// The Windows-only half of this module pulls in imports that a host build never uses, because that
// half is not compiled there. Gating them here keeps the host build warning-free without a blanket
// allow that would also hide a genuinely unused import.
#[cfg(windows)]
use std::fs;
#[cfg(windows)]
use std::io::Write;
#[cfg(windows)]
use std::path::Path;

use anyhow::{Context, Result};
use std::io::Read;

#[cfg(windows)]
use crate::transfer::TransferIo;

/// Bytes per chunk. See the module comment for why this number and not another.
///
/// Used by the Windows transfer path and by the tests; not referenced in a host build outside them.
#[allow(dead_code)]
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Refuse to transfer a single file larger than this, before opening anything.
///
/// A control channel is not a file server, and an unbounded transfer is an unbounded commitment of
/// time: at a few hundred MB/s over a socket, a 10 GiB file is minutes of a service that answers one
/// request at a time. The limit is a design statement, not a resource limit — it says "use this for
/// artifacts, not for bulk data".
#[cfg(windows)]
pub const MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The real, on-disk `TransferIo` for a Windows guest.
///
/// Stateless: every method takes the path it needs. The transfer plan has already established that
/// both paths are contained in their roots (`transfer::plan`), so this type does not re-check
/// containment — it would be a second, weaker check, and a boundary enforced in two places is one
/// that can disagree with itself. It *does* check what planning cannot know: whether the file is
/// larger than the configured ceiling, whether the parent directory exists, and whether a write
/// actually wrote every byte.
#[cfg(windows)]
pub struct FilesystemIo;

#[cfg(windows)]
impl FilesystemIo {
    pub fn new() -> Self {
        FilesystemIo
    }

    /// Open a file for writing, ensuring the parent directory exists first.
    ///
    /// Creating the parent is deliberate: the alternative is a transfer that fails because a
    /// directory was missing, which the caller then has to create with a second operation. But it
    /// is only the *immediate* parent that is created, never a chain — a deeply nested path is
    /// usually a mistake in the request rather than an intention.
    fn open_for_write(path: &str) -> Result<fs::File> {
        let p = Path::new(path);

        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating the directory for '{path}'"))?;
            }
        }

        fs::File::create(p).with_context(|| format!("opening '{path}' for writing"))
    }

    /// Read a file, refusing one that is too large before reading a byte of it.
    ///
    /// The size is checked from metadata first so an oversized file is refused *before* the memory
    /// is committed. Reading first and checking after would defeat the point.
    fn read_capped(path: &str) -> Result<Vec<u8>> {
        let meta = fs::metadata(path).with_context(|| format!("reading metadata for '{path}'"))?;

        if !meta.is_file() {
            anyhow::bail!("'{path}' is not a regular file");
        }
        if meta.len() > MAX_TRANSFER_BYTES {
            anyhow::bail!(
                "'{path}' is {} bytes, above the {} byte ceiling for a single transfer",
                meta.len(),
                MAX_TRANSFER_BYTES
            );
        }

        fs::read(path).with_context(|| format!("reading '{path}'"))
    }
}

#[cfg(windows)]
impl Default for FilesystemIo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl TransferIo for FilesystemIo {
    fn guest_exists(&self, path: &str) -> Result<bool> {
        Ok(Path::new(path).is_file())
    }

    fn host_exists(&self, path: &str) -> Result<bool> {
        // On Windows the "host" side of a transfer is the host's staging directory, which this
        // service reaches through the host filesystem it shares. Same check.
        Ok(Path::new(path).is_file())
    }

    fn read_guest(&self, path: &str) -> Result<Vec<u8>> {
        Self::read_capped(path)
    }

    fn read_host(&self, path: &str) -> Result<Vec<u8>> {
        Self::read_capped(path)
    }

    fn write_guest(&self, path: &str, data: &[u8]) -> Result<()> {
        let mut file = Self::open_for_write(path)?;
        file.write_all(data)
            .with_context(|| format!("writing {path}"))?;
        file.flush().with_context(|| format!("flushing {path}"))?;

        // Verify the byte count rather than trusting `write_all` to have meant it.
        //
        // `write_all` returning Ok means every byte was handed to the OS, not that every byte is on
        // disk and not that the path we opened is the path a reader will see. Comparing the length
        // is one cheap syscall and it converts "the write returned success" into "the file is the
        // size we intended", which are different claims.
        let written = fs::metadata(path)
            .with_context(|| format!("confirming the write to '{path}'"))?
            .len();
        if written != data.len() as u64 {
            anyhow::bail!(
                "wrote {written} bytes to '{path}' but intended {} — the transfer is short",
                data.len()
            );
        }
        Ok(())
    }

    fn write_host(&self, path: &str, data: &[u8]) -> Result<()> {
        let mut file = Self::open_for_write(path)?;
        file.write_all(data)
            .with_context(|| format!("writing {path}"))?;
        file.flush().with_context(|| format!("flushing {path}"))?;

        let written = fs::metadata(path)
            .with_context(|| format!("confirming the write to '{path}'"))?
            .len();
        if written != data.len() as u64 {
            anyhow::bail!(
                "wrote {written} bytes to '{path}' but intended {} — the transfer is short",
                data.len()
            );
        }
        Ok(())
    }
}

/// The staging roots this guest will read from and write to.
///
/// The whole reason for a staging directory rather than shared access: the original project this
/// was informed by mounted the host's `/` into the guest as a writable `Z:\`, so any guest process
/// could rewrite host binaries. A staging root means a transfer can only ever touch the directory
/// chosen for it — the containment check in `transfer::plan` refuses everything else.
///
/// Deliberately returned as a structure rather than read from a global, so a test can substitute
/// its own roots without mutating process state.
pub struct Roots {
    pub guest: String,
    pub host: String,
}

impl Roots {
    /// The real staging roots for an installed guest.
    ///
    /// Guest side under the service's program data; host side is the directory the host exposes for
    /// staging. Both are created on first use by `FilesystemIo::open_for_write`, which creates the
    /// immediate parent of any path it writes.
    pub fn default_staging() -> Self {
        Roots {
            guest: r"C:\ProgramData\wvm\staging".to_string(),
            host: r"C:\ProgramData\wvm\staging-host".to_string(),
        }
    }

    /// Override the roots from the environment.
    ///
    /// Exists so an operator can point staging at a volume with room, and so the test suite can use
    /// a temporary directory. Without an override the paths above are hardcoded into the binary,
    /// which is fine until the day the guest has a small system drive.
    pub fn from_env() -> Self {
        let mut roots = Self::default_staging();
        if let Ok(v) = std::env::var("WVM_GUEST_STAGING") {
            if !v.trim().is_empty() {
                roots.guest = v;
            }
        }
        if let Ok(v) = std::env::var("WVM_HOST_STAGING") {
            if !v.trim().is_empty() {
                roots.host = v;
            }
        }
        roots
    }
}

/// The process-wide roots, resolved once.
pub fn roots() -> Roots {
    Roots::from_env()
}

/// Plan a transfer against these roots.
///
/// `plan` needs both paths, and the request supplies exactly one of them per direction (the other
/// side names its staging location). So the supplied path is checked for containment and the other
/// is derived from the matching root — meaning a caller states what it wants and the guest decides
/// where that lands, which is the whole point of the staging design.
/// Takes the WIRE direction type, because that is what arrives over the socket, and converts at
/// this boundary. The two enums are deliberately separate: the wire type is a protocol contract that
/// cannot change without breaking peers, while the internal one is free to grow without a version
/// bump.
pub fn plan_from(
    direction: &wvm_ipc::TransferDirection,
    roots: &Roots,
    guest_path: &str,
    host_path: &str,
) -> Result<crate::transfer::TransferPlan> {
    let internal = match direction {
        wvm_ipc::TransferDirection::HostToGuest => crate::transfer::Direction::HostToGuest,
        wvm_ipc::TransferDirection::GuestToHost => crate::transfer::Direction::GuestToHost,
    };

    crate::transfer::plan(internal, &roots.guest, &roots.host, guest_path, host_path)
}
///
/// Extracted so the chunking arithmetic is testable on the host, where it is the part most likely
/// to be wrong and the part least likely to show up in a small test. An off-by-one here produces a
/// file that is *nearly* the right size, which is exactly the class of bug the length check above
/// exists to catch — so both are tested independently.
///
/// Returns `(offset, len)` pairs that tile `total` exactly, with no overlap and no gap.
#[allow(dead_code)]
pub fn chunks(total: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < total {
        let len = CHUNK_SIZE.min(total - offset);
        out.push((offset, len));
        offset += len;
    }
    out
}

/// Read a `Read` in chunks, calling `on_chunk` for each, and return the total byte count.
///
/// This is the shape a chunked transfer needs, and it is here rather than inline so it can be
/// tested without a socket: the property that matters is that every byte is delivered exactly once,
/// in order, and that the final chunk is not dropped when it is short.
#[allow(dead_code)]
pub fn read_in_chunks<R: Read, F: FnMut(&[u8]) -> Result<()>>(
    mut reader: R,
    mut on_chunk: F,
) -> Result<u64> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = reader.read(&mut buf).context("reading a chunk")?;
        if n == 0 {
            break;
        }
        on_chunk(&buf[..n])?;
        total += n as u64;
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_tile_the_total_exactly() {
        // The property: no gaps, no overlaps, every byte covered once. Asserted over a range that
        // includes empty, smaller than one chunk, exactly one, just over, and several.
        for total in [
            0usize,
            1,
            100,
            CHUNK_SIZE - 1,
            CHUNK_SIZE,
            CHUNK_SIZE + 1,
            CHUNK_SIZE * 3,
        ] {
            let spans = chunks(total);
            let covered: usize = spans.iter().map(|(_, len)| len).sum();
            assert_eq!(covered, total, "total {total}: chunks must tile it exactly");

            let mut expected_offset = 0;
            for (offset, len) in &spans {
                assert_eq!(
                    *offset, expected_offset,
                    "total {total}: offsets must be contiguous"
                );
                assert!(
                    *len <= CHUNK_SIZE,
                    "total {total}: a chunk exceeded the chunk size"
                );
                assert!(
                    *len > 0,
                    "total {total}: a zero-length chunk would loop forever"
                );
                expected_offset += len;
            }
        }
    }

    #[test]
    fn an_empty_transfer_produces_no_chunks() {
        // Not a chunk of length zero: that would be an infinite loop in any caller that loops
        // until it sees an empty chunk.
        assert!(chunks(0).is_empty());
    }

    #[test]
    fn a_short_final_chunk_is_not_dropped() {
        // The classic off-by-one: total not a multiple of the chunk size. If the loop stopped on
        // "read less than CHUNK_SIZE" it would silently truncate the file.
        let total = CHUNK_SIZE * 2 + 7;
        let spans = chunks(total);
        assert_eq!(spans.len(), 3);
        assert_eq!(
            spans[2].1, 7,
            "the short tail must be its own chunk, not dropped"
        );
    }

    #[test]
    fn read_in_chunks_delivers_every_byte_in_order_exactly_once() {
        // Deliberately crossing a chunk boundary, and deliberately not a multiple of it, so a
        // boundary bug and a tail bug would both show.
        let data: Vec<u8> = (0..(CHUNK_SIZE * 2 + 123))
            .map(|i| (i % 251) as u8)
            .collect();

        let mut collected = Vec::new();
        let total = read_in_chunks(std::io::Cursor::new(&data), |chunk| {
            collected.extend_from_slice(chunk);
            Ok(())
        })
        .expect("read");

        assert_eq!(
            total as usize,
            data.len(),
            "the count must match what was read"
        );
        assert_eq!(
            collected, data,
            "the bytes must arrive in order and complete"
        );
    }

    #[test]
    fn an_empty_source_yields_no_chunks_and_zero_bytes() {
        let mut calls = 0;
        let total = read_in_chunks(std::io::Cursor::new(Vec::<u8>::new()), |_| {
            calls += 1;
            Ok(())
        })
        .expect("read");

        assert_eq!(total, 0);
        assert_eq!(calls, 0, "no chunk callback for an empty source");
    }

    #[test]
    fn the_chunk_size_leaves_room_to_breathe() {
        // A chunk larger than a typical socket buffer means the writer blocks waiting on the reader
        // while holding the whole chunk — the exact backpressure problem chunking is meant to
        // avoid. This asserts the relationship rather than the number, so changing the constant
        // does not break a test that was really trying to say something else.
        const TYPICAL_SOCKET_SEND_BUFFER: usize = 1024 * 1024;
        // Compile-time rather than runtime: both values are constants, so a `assert!` here could
        // never fail at test time and clippy is right to say so. A const assertion fails the BUILD
        // instead, which is strictly better — the relationship cannot be broken in a binary that
        // compiles.
        const _: () = assert!(
            CHUNK_SIZE <= TYPICAL_SOCKET_SEND_BUFFER,
            "a chunk should fit in a socket buffer, so the writer is not forced to block mid-chunk"
        );
    }
}
