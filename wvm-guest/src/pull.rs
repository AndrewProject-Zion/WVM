// This module's real work is Windows-side: the guest reads its own files and serves them. On a
// host build that half is not compiled, so its types and helpers read as unused there while
// being exercised in a real guest. The tests below run on the host and cover the offset and
// length arithmetic, which is the part most likely to be wrong. Same pattern as chunk.rs.
#![allow(dead_code)]

//! The guest side of a chunked pull: serve the bytes, one acknowledged chunk at a time.
//!
//! # Why this is asymmetric with push, and why that matters
//!
//! A push has the host as the READER: it reads its own file, encodes, and sends. A pull has the
//! guest as the reader: the host asks for a chunk, the guest reads it from disk, encodes, and
//! replies. The bytes travel in the same framing but the *initiative* is opposite.
//!
//! An earlier version of `transfer::plan` treated the two as one mirrored operation, and that
//! assumption is exactly what hid a bug: it checked the host's path against a root the guest cannot
//! see, so every honest transfer was refused with a correct-looking error. Writing pull as its own
//! path rather than an inversion of push is the correction.
//!
//! # Lockstep in the other direction
//!
//! The guest does not stream ahead either, for the same reason the host does not: the reader here
//! is faster than the writer at the other end. Each request names the offset and the length wanted,
//! and the reply carries exactly that. The sender's disk sets the pace, and memory stays flat at any
//! file size.
//!
//! # Why the offset and length are requested rather than implied
//!
//! A "give me the next chunk" protocol would need the guest to remember how far it had served, and
//! a retry after a lost reply would serve the wrong bytes. Naming the offset makes every request
//! self-describing and makes a replay harmless: the same request produces the same bytes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

/// Largest file this will serve.
///
/// The same ceiling as a push, for the same reason: a control channel is not a file server. Stated
/// as its own constant rather than shared, because the two could reasonably diverge — a guest may be
/// happy to receive a large artifact it will never send back.
pub const MAX_PULL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// A file currently being served to the host.
struct OpenSource {
    file: File,
    path: PathBuf,
    /// Total size, read once at open. Served from here rather than re-stat'd per chunk so a file
    /// that changes size mid-transfer cannot make the arithmetic disagree with itself.
    size: u64,
}

static OPEN: Mutex<Option<HashMap<PathBuf, OpenSource>>> = Mutex::new(None);

fn with_open<T>(f: impl FnOnce(&mut HashMap<PathBuf, OpenSource>) -> T) -> T {
    let mut guard = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// A source opened for reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub size: u64,
}

/// Open a file to serve to the host.
///
/// Refuses a directory, a missing file, and anything larger than the ceiling: all three are
/// answered here, before the host has committed to receiving anything. A pull that fails after the
/// first chunk has already cost the round trip and left the host with a partial local file.
pub fn begin_source(path_in_root: &str, ceiling: u64) -> Result<Source> {
    let path = PathBuf::from(path_in_root);

    let meta = std::fs::metadata(&path)
        .with_context(|| format!("reading metadata for {}", path.display()))?;

    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > ceiling {
        bail!(
            "{} is {} bytes, above the {} byte ceiling for a single transfer",
            path.display(),
            meta.len(),
            ceiling
        );
    }

    let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;

    let source = Source {
        path: path.clone(),
        size: meta.len(),
    };

    with_open(|m| {
        m.insert(
            path.clone(),
            OpenSource {
                file,
                path: path.clone(),
                size: meta.len(),
            },
        );
    });

    Ok(source)
}

/// Read one chunk at `offset`, up to `length` bytes.
///
/// Returns the bytes and whether they reach the end of the file. The guest never decides how much
/// to send beyond the requested length: the caller's buffer is its own business, and a guest that
/// sent more than asked would make the host's accounting wrong.
pub fn read_chunk(path_in_root: &str, offset: u64, length: u64) -> Result<(Vec<u8>, bool, u64)> {
    with_open(|m| {
        let path = PathBuf::from(path_in_root);
        let Some(src) = m.get_mut(&path) else {
            bail!("no pull is open for {}", path.display());
        };

        if offset > src.size {
            bail!(
                "chunk requested at offset {offset} but {} is only {} bytes",
                src.path.display(),
                src.size
            );
        }

        src.file
            .seek(SeekFrom::Start(offset))
            .with_context(|| format!("seeking to {offset} in {}", src.path.display()))?;

        // Read up to `length`, but never past the end of the file. The last chunk is short, and
        // asking for a full chunk there must not produce a padded reply.
        let remaining = src.size - offset;
        let want = length.min(remaining) as usize;

        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match src.file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading {} at {offset}", src.path.display()));
                }
            }
        }
        buf.truncate(filled);

        let eof = offset + filled as u64 >= src.size;
        let total = src.size;

        // Close on the last chunk, so a second pull of the same file starts cleanly rather than
        // continuing from a stale position.
        if eof {
            m.clear();
        }

        Ok((buf, eof, total))
    })
}

/// The path of the single source currently open, if there is exactly one.
///
/// A `PullChunk` carries no path — deliberately, because naming the file per chunk would be
/// redundant bytes and one more place for the two sides to disagree — so the guest has to find the
/// open source instead. More than one open means the caller has broken the lockstep protocol rather
/// than that it is doing something clever, and that is reported rather than guessed at.
pub fn only_open_source() -> Result<String> {
    with_open(|m| match m.len() {
        0 => bail!("no pull is open; send Transfer with direction guest_to_host first"),
        1 => {
            let (path, _) = m.iter().next().expect("len checked");
            Ok(path.to_string_lossy().to_string())
        }
        n => bail!("{n} pulls are open; the lockstep protocol allows one at a time"),
    })
}

/// Abandon a pull. Nothing to clean up on disk — the source is the guest's own file and is only
/// being read — but the handle must be released so a later pull can reopen it.
pub fn end_source(path_in_root: &str) -> Result<()> {
    with_open(|m| {
        m.remove(&PathBuf::from(path_in_root));
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests: they share the module's static map, which is correct for the guest —
    /// the protocol allows one transfer at a time — but means a parallel run has them clearing each
    /// other's state.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn reset() -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        with_open(|m| m.clear());
        guard
    }

    fn scratch(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wvm-pull-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let p = dir.join(name);
        std::fs::write(&p, contents).expect("seed");
        p
    }

    #[test]
    fn chunks_reassembled_in_order_reproduce_the_file() {
        let _serial = reset();
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let src = scratch("in-order.bin", &data);

        let opened = begin_source(src.to_str().unwrap(), u64::MAX).expect("open");
        assert_eq!(opened.size, 1000);

        // Serve in 400-byte chunks so the last one is short.
        let mut reassembled = Vec::new();
        let mut offset = 0u64;
        loop {
            let (chunk, eof, _total) =
                read_chunk(src.to_str().unwrap(), offset, 400).expect("read");
            reassembled.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
            if eof {
                break;
            }
        }

        assert_eq!(
            reassembled, data,
            "the reassembled bytes must match the source"
        );

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn the_final_chunk_is_short_rather_than_padded() {
        // The property that catches an off-by-one at the end: asking for a full chunk when fewer
        // bytes remain must return only what exists. A padded reply would produce a file longer
        // than the original, with trailing zeros.
        let _serial = reset();
        let data = b"0123456789".to_vec();
        let src = scratch("short.bin", &data);
        begin_source(src.to_str().unwrap(), u64::MAX).expect("open");

        let (chunk, eof, total) = read_chunk(src.to_str().unwrap(), 6, 4096).expect("read");
        assert_eq!(chunk, b"6789", "only the bytes that exist");
        assert!(eof, "the end of the file is reached");
        assert_eq!(total, 10);

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn reading_past_the_end_is_refused() {
        // An offset beyond the file is a caller bug, not a request for an empty chunk. Refusing
        // makes it visible; returning empty bytes would look like a successful read of nothing.
        let _serial = reset();
        let src = scratch("past-end.bin", b"short");
        begin_source(src.to_str().unwrap(), u64::MAX).expect("open");

        let err = read_chunk(src.to_str().unwrap(), 999, 100).expect_err("must be refused");
        assert!(err.to_string().contains("only 5 bytes"), "{err}");

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn a_chunk_with_no_open_source_is_refused() {
        let _serial = reset();
        let err = read_chunk("C:/nowhere.bin", 0, 100).expect_err("must be refused");
        assert!(err.to_string().contains("no pull is open"), "{err}");
    }

    #[test]
    fn a_file_above_the_ceiling_is_refused_at_open() {
        // Refused before any bytes move. A pull that failed after the first chunk has already cost
        // the round trip and left the host holding a partial file.
        let _serial = reset();
        let src = scratch("big.bin", &vec![0u8; 2048]);

        let err = begin_source(src.to_str().unwrap(), 1024).expect_err("must be refused");
        assert!(
            err.to_string().contains("above the 1024 byte ceiling"),
            "{err}"
        );

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn an_empty_file_serves_one_empty_final_chunk() {
        // The boundary below the interesting one. The host must still get an `eof` reply, or it
        // would wait forever for a zero-byte file.
        let _serial = reset();
        let src = scratch("empty.bin", b"");
        let opened = begin_source(src.to_str().unwrap(), u64::MAX).expect("open");
        assert_eq!(opened.size, 0);

        let (chunk, eof, total) = read_chunk(src.to_str().unwrap(), 0, 4096).expect("read");
        assert!(chunk.is_empty());
        assert!(eof, "an empty file is immediately at its end");
        assert_eq!(total, 0);

        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let _serial = reset();
        let dir = std::env::temp_dir().join(format!("wvm-pull-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");

        let err = begin_source(dir.to_str().unwrap(), u64::MAX).expect_err("must be refused");
        assert!(err.to_string().contains("not a regular file"), "{err}");

        let _ = std::fs::remove_dir(&dir);
    }
}
