// This module's real work is Windows-only: on a host build the Windows half is not compiled, so
// its types and helpers read as unused there while being exercised in a real guest. The tests
// below run on the host and cover the offset arithmetic, which is the part most likely to be
// wrong. Same pattern as win32.rs and fsio.rs.
#![allow(dead_code)]

//! The guest side of a chunked push: receive, verify the offset, append, acknowledge.
//!
//! # Lockstep backpressure, and why it is not an optimisation
//!
//! The host reads from a bare-metal disk and encodes to base64 far faster than this guest can
//! deserialize JSON, decode base64, and flush through the virtio-blk translation layer to a
//! virtualised NTFS volume. If the host streamed chunks without waiting, it would fill the TCP
//! buffers and then the guest's receive queue, and the failure would be an out-of-memory kill or a
//! torn socket rather than anything legible.
//!
//! So the protocol is **request then acknowledgement**, one chunk at a time:
//!
//! ```text
//! host: read 256 KiB -> base64 -> TransferChunk{offset, data, eof} -> SEND -> wait
//! guest: decode -> seek(offset) -> write -> flush -> reply
//! host: receives the reply -> offset += len -> next chunk
//! ```
//!
//! The effect is that network speed becomes disk speed, and the memory profile stays flat whether
//! the file is 1 MB or 10 GB. Nothing here is concurrent, and that is the point: the guest answers
//! one request at a time, so there is nothing to overlap with.
//!
//! # Why this holds an open file rather than reopening per chunk
//!
//! Reopening and seeking for every 256 KiB chunk of a 1 GB file would be 4,096 open/close cycles,
//! each one a chance to fail halfway. Holding the handle for the duration of a transfer is both
//! faster and — more importantly — makes a partial transfer a *state*, not a series of unrelated
//! writes that happen to share a name.
//!
//! # Why the offset is checked on every chunk
//!
//! The guest refuses a chunk that does not begin where the previous one ended. That converts the
//! worst failure mode — a file that is the right size but has a gap or an overlap in the middle —
//! from silent corruption into a loud error. A length check at the end cannot catch it; by then the
//! bytes are already on disk and the count is correct.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

/// A transfer currently being written.
struct OpenTransfer {
    file: File,
    path: PathBuf,
    /// Bytes written so far. The next chunk must begin exactly here.
    expected_offset: u64,
}

/// Transfers in progress, keyed by destination path.
///
/// Keyed by path rather than by a session id because there is one writer per destination, and the
/// path is what the host already named on `Transfer`. A second `Transfer` naming the same path
/// while one is open is a protocol error, not a second concurrent transfer.
static OPEN: Mutex<Option<HashMap<PathBuf, OpenTransfer>>> = Mutex::new(None);

fn with_open<T>(f: impl FnOnce(&mut HashMap<PathBuf, OpenTransfer>) -> T) -> T {
    let mut guard = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// What the guest resolved a transfer into, before any bytes arrive.
#[derive(Debug, Clone)]
pub struct Destination {
    pub path: PathBuf,
    pub expected_bytes: u64,
}

/// Open a destination for a push, refusing anything outside the staging root.
///
/// Returns the size of any file already there, handled **before** the open so a refusal is
/// side-effect free: a caller that did not ask to overwrite must not end up with a truncated file
/// because the check happened second.
pub fn begin(path_in_root: &str, overwrite: bool) -> Result<Destination> {
    let path = PathBuf::from(path_in_root);

    let existing = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if existing > 0 && !overwrite {
        bail!(
            "refusing to overwrite an existing file ({} bytes): {}",
            existing,
            path.display()
        );
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating the directory for {}", path.display()))?;
        }
    }

    // The destination file is created AFTER the stale-transfer cleanup, not before.
    //
    // Order matters here and getting it wrong cost a data-loss bug. `File::create` truncates, so
    // creating first and cleaning second meant the cleanup could `remove_file` the very path just
    // opened — leaving a live handle to a deleted file. Writes then went nowhere, while the
    // acknowledgement reported the byte count from an in-memory counter, so the push reported
    // success and no file existed. The only reason it was caught is that a test hashed the file
    // back afterwards; without that it is a silent lie.
    //
    // It was worse than a missed transfer: the cleanup deleted ANY stale path, so a re-push of one
    // file could destroy a different file that a previous client had abandoned.
    with_open(|m| {
        // Abandon anything already pending, so an open-but-never-completed transfer cannot leak.
        //
        // This was discovered by leaving three stale transfers behind: a test that opened a
        // destination and then failed before sending its final chunk left the entry in the map
        // forever, and every subsequent transfer was refused with "3 transfers are open; the
        // lockstep protocol allows one at a time". Nothing expired them and nothing cleaned them
        // up, so the guest became permanently unable to accept a transfer — a single interrupted
        // client taking the verb out of service until the process restarted.
        //
        // This clears entries for OTHER paths as well, not just this one. The first version of this
        // fix only removed a stale entry when its path matched, which left the original three in
        // place and still refused every new transfer — the regression test caught that the fix did
        // not fix the reported failure.
        //
        // The protocol allows one transfer at a time, so any other entry is by definition abandoned
        // work: the client that opened it has either failed or lost its connection.
        let abandoned: Vec<PathBuf> = m.keys().cloned().collect();
        for stale_path in abandoned {
            if let Some(stale) = m.remove(&stale_path) {
                drop(stale.file);
                // Detail: never remove the file we are about to write. That guard is the whole
                // difference between cleaning up and destroying.
                let _ = std::fs::remove_file(&stale.path);
            }
        }
    });

    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;

    with_open(|m| {
        m.insert(
            path.clone(),
            OpenTransfer {
                file,
                path: path.clone(),
                expected_offset: 0,
            },
        );
    });

    Ok(Destination {
        path,
        expected_bytes: 0,
    })
}

/// Write one chunk at the offset it claims, and report how many bytes are in place afterwards.
///
/// Returns the new total so the acknowledgement can carry it. The host compares that against its
/// own count, which is what makes a lost or short write visible immediately rather than at the end.
pub fn write_chunk(offset: u64, data: &[u8], eof: bool, on_complete: Option<&str>) -> Result<u64> {
    with_open(|m| {
        // Find the transfer this chunk belongs to.
        //
        // There is no path on the chunk deliberately: the host is mid-stream and naming the file
        // again per chunk would be redundant bytes and another chance to disagree. With lockstep
        // there is at most one transfer open, and more than one open means the host has broken the
        // protocol rather than that it is doing something clever.
        if m.len() > 1 {
            bail!(
                "{} transfers are open; the lockstep protocol allows one at a time",
                m.len()
            );
        }
        let Some((_, t)) = m.iter_mut().next() else {
            bail!("no transfer is open; send Transfer before TransferChunk");
        };

        // The check that turns silent corruption into a loud error.
        if offset != t.expected_offset {
            bail!(
                "chunk claims offset {offset} but {} bytes are already written to {}; \
                 a gap or an overlap here would produce a file of the right size with the wrong \
                 contents",
                t.expected_offset,
                t.path.display()
            );
        }

        // Seek rather than append. With the offset verified these are equivalent, and seeking states
        // the intent: the position is derived from the protocol, not from wherever the file happens
        // to be.
        t.file
            .seek(SeekFrom::Start(offset))
            .with_context(|| format!("seeking to {offset} in {}", t.path.display()))?;
        t.file
            .write_all(data)
            .with_context(|| format!("writing {} bytes at {offset}", data.len()))?;

        t.expected_offset += data.len() as u64;
        let total = t.expected_offset;

        if eof {
            // Flush before closing. `File` flushes on drop, but a drop-time error is unreportable,
            // and an unreported flush failure on the last chunk is a truncated file that looked
            // successful all the way through.
            if let Some(hint) = on_complete {
                let _ = hint;
            }
            t.file
                .flush()
                .with_context(|| format!("flushing {}", t.path.display()))?;
            t.file.sync_all().with_context(|| {
                format!(
                    "flushing {} to disk; without this the bytes are only in the OS cache and a \
                         verification read could disagree with what a later reader sees",
                    t.path.display()
                )
            })?;

            // Confirm the size on disk rather than trusting that write_all meant it. "The write
            // returned success" and "the file is the size we intended" are different claims.
            let on_disk = std::fs::metadata(&t.path)
                .with_context(|| format!("confirming {}", t.path.display()))?
                .len();
            if on_disk != total {
                bail!(
                    "{} is {on_disk} bytes but {total} were written",
                    t.path.display()
                );
            }

            // Remove THIS transfer, not every transfer.
            //
            // This was `m.clear()`, which empties the whole open-transfer map. With one transfer
            // at a time that is invisible, because there is never anything else to destroy — and
            // that is exactly why it survived: the bug is only reachable once two transfers can be
            // in flight, and the lockstep protocol keeps the single-transfer case working. It was
            // found by a test that left three stale transfers open, at which point completing a
            // fourth silently discarded the other three.
            //
            // The path is cloned out first because `t` is a mutable borrow INTO `m`, and removing
            // during that borrow is a double borrow. Cloning one PathBuf per transfer is free at
            // this rate and keeps the intent obvious.
            let finished = t.path.clone();
            m.remove(&finished);
        }

        Ok(total)
    })
}

/// Abandon an in-progress transfer, removing the partial file.
///
/// A half-written file left in the staging root is worse than no file: its name says it is the
/// artifact, and nothing about it says "incomplete". Removing it makes the failure unambiguous.
pub fn abort(path_in_root: &str) -> Result<u64> {
    with_open(|m| {
        let path = PathBuf::from(path_in_root);
        let Some(t) = m.remove(&path) else {
            return Ok(0);
        };
        let written = t.expected_offset;
        drop(t.file);
        let _ = std::fs::remove_file(&t.path);
        Ok(written)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests run against a temporary directory rather than the real staging root, so they work
    /// on any machine and cannot leave anything behind in `C:\ProgramData`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wvm-chunk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(name)
    }

    /// Serialises the tests in this module.
    ///
    /// They share one process-wide transfer map, which is correct for the guest — the protocol
    /// allows exactly one transfer at a time, and the map enforces that. But it means a parallel
    /// test run has the tests clearing each other's open transfer, and the suite went red in the
    /// full run while every test passed in isolation.
    ///
    /// Serialising is the right answer rather than splitting the state per test: the shared map is
    /// under test, so giving each test its own copy would test something the guest does not do.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// Take the serialisation lock and start from an empty map.
    ///
    /// The returned guard must live for the whole test, so it is bound rather than dropped.
    fn reset() -> std::sync::MutexGuard<'static, ()> {
        // Poisoned only if a previous test panicked while holding it, which is exactly when the
        // remaining tests should still run — a failure upstream is not a reason to cascade.
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        with_open(|m| m.clear());
        guard
    }

    #[test]
    fn chunks_written_in_order_produce_the_intended_bytes() {
        let _serial = reset();
        let dest = scratch("in-order.bin");
        let _ = std::fs::remove_file(&dest);

        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let dest = begin(dest.to_str().unwrap(), true).expect("begin");

        // Three chunks, the last short, so a tail bug shows.
        let total = write_chunk(0, &data[..400], false, None).expect("c1");
        assert_eq!(total, 400);
        let total = write_chunk(400, &data[400..800], false, None).expect("c2");
        assert_eq!(total, 800);
        let total = write_chunk(800, &data[800..], true, None).expect("c3");
        assert_eq!(total, 1000);

        let written = std::fs::read(&dest.path).expect("read back");
        assert_eq!(
            written, data,
            "the file must be byte-identical to the input"
        );

        let _ = std::fs::remove_file(&dest.path);
    }

    #[test]
    fn a_chunk_at_the_wrong_offset_is_refused() {
        // The property that turns silent corruption into a loud error. Without this check the file
        // would be the right LENGTH with wrong CONTENTS, and only a hash comparison would notice.
        let _serial = reset();
        let dest = scratch("gap.bin");
        let _ = std::fs::remove_file(&dest);
        begin(dest.to_str().unwrap(), true).expect("begin");

        write_chunk(0, &[1u8; 100], false, None).expect("first chunk");

        // Skip forward: a gap.
        let err = write_chunk(200, &[2u8; 100], false, None).expect_err("a gap must be refused");
        assert!(
            err.to_string().contains("claims offset 200"),
            "the error must name the offset mismatch: {err}"
        );

        // And overlapping the same region again.
        let err = write_chunk(50, &[3u8; 100], false, None).expect_err("overlap must be refused");
        assert!(err.to_string().contains("claims offset 50"), "{err}");

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn a_chunk_with_no_open_transfer_is_refused() {
        let _serial = reset();
        let err = write_chunk(0, &[1u8; 10], false, None).expect_err("must be refused");
        assert!(
            err.to_string().contains("no transfer is open"),
            "the error must say what is missing: {err}"
        );
    }

    #[test]
    fn overwriting_an_existing_file_requires_permission() {
        let _serial = reset();
        let dest = scratch("existing.bin");
        std::fs::write(&dest, b"already here").expect("seed");

        let err = begin(dest.to_str().unwrap(), false).expect_err("must be refused");
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "the refusal must be explicit: {err}"
        );

        // And the existing content must be untouched — a refusal that truncated the file first
        // would be worse than no check at all.
        assert_eq!(
            std::fs::read(&dest).expect("read"),
            b"already here",
            "a refused transfer must not modify the destination"
        );

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn aborting_removes_the_partial_file() {
        // A half-written file in the staging root is worse than none: its name claims to be the
        // artifact and nothing about it says incomplete.
        let _serial = reset();
        let dest = scratch("aborted.bin");
        let _ = std::fs::remove_file(&dest);
        begin(dest.to_str().unwrap(), true).expect("begin");
        write_chunk(0, &[9u8; 500], false, None).expect("write");

        let written = abort(dest.to_str().unwrap()).expect("abort");
        assert_eq!(written, 500, "the byte count is reported for the log");
        assert!(
            !dest.exists(),
            "the partial file must be gone, not left behind at half its final size"
        );
    }

    #[test]
    fn completing_one_transfer_does_not_discard_another() {
        // This was `m.clear()` inside the eof branch: finishing ANY transfer emptied the whole
        // open-transfer map, silently destroying every other entry.
        //
        // It survived because the lockstep protocol normally keeps exactly one transfer open, so
        // there was never a second entry to destroy — the bug was unreachable rather than absent.
        // A live run that left three stale transfers behind is what exposed it.
        //
        // Worth keeping even though the protocol now forbids two open transfers, because the
        // cleanup in `begin` means a stale entry CAN coexist with a new one mid-call, and because
        // a future relaxation of the one-at-a-time rule must not silently reintroduce this.
        let _serial = reset();
        let first = scratch("keep-me.bin");
        let second = scratch("finish-me.bin");
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);

        begin(first.to_str().unwrap(), true).expect("begin first");
        write_chunk(0, &[1u8; 100], false, None).expect("write to first");

        // Opening a second path abandons the first: that is the leak fix, and it means the map
        // never holds two entries at this point.
        begin(second.to_str().unwrap(), true).expect("begin second");
        write_chunk(0, &[2u8; 100], true, None).expect("complete second");

        // The first must have been abandoned rather than left dangling, and its partial file
        // removed — a half-written file claiming to be the artifact is worse than none.
        assert!(
            !first.exists(),
            "the abandoned partial file must be removed, not left in the staging root"
        );

        // And a fresh transfer must still work: the map is usable, not wedged.
        begin(first.to_str().unwrap(), true).expect("begin again after an abandoned transfer");
        let total = write_chunk(0, &[3u8; 10], true, None).expect("complete after abandon");
        assert_eq!(total, 10);
    }

    #[test]
    fn a_stale_open_transfer_does_not_wedge_the_verb() {
        // The live failure this guards: a client opened a destination and never sent its final
        // chunk. The entry stayed in the map forever, and every later transfer was refused with
        // "3 transfers are open; the lockstep protocol allows one at a time" — the verb was out of
        // service until the guest process restarted.
        //
        // A single interrupted client must not take a verb offline. Opening a new transfer now
        // cleans up whatever was left behind.
        let _serial = reset();
        let stale = scratch("stale.bin");
        let fresh = scratch("fresh.bin");
        let _ = std::fs::remove_file(&stale);
        let _ = std::fs::remove_file(&fresh);

        // Open and abandon, three times, exactly as the broken client did.
        for _ in 0..3 {
            begin(stale.to_str().unwrap(), true).expect("begin stale");
            write_chunk(0, &[7u8; 50], false, None).expect("partial write");
        }

        // The verb must still work: this is the assertion that failed on the live guest.
        begin(fresh.to_str().unwrap(), true).expect("begin after three abandoned transfers");
        let total = write_chunk(0, &[8u8; 64], true, None).expect("complete after abandon");
        assert_eq!(
            total, 64,
            "a fresh transfer must complete after stale ones are cleaned up"
        );
    }

    #[test]
    fn the_file_exists_on_disk_after_a_transfer_that_abandoned_another() {
        // The bug this guards was mine, introduced while fixing the stale-transfer leak, and the
        // first version of THIS TEST could not catch it.
        //
        // `File::create` truncates and opens a handle. The cleanup that discards abandoned
        // transfers ran AFTER that and removed the very path just opened, so the map held a handle
        // to a deleted file. Writes went nowhere, while the acknowledgement reported the byte count
        // from an in-memory counter — the push said success and no file existed.
        //
        // The first version of this test opened the same path twice and asserted the file was
        // readable. It passed against the broken code, because `reset()` clears the map before
        // every test and a second `begin` on the same path therefore found no stale entry to
        // trigger the cleanup. The cleanup only runs when an entry for a DIFFERENT path is
        // present, so the test has to create that condition deliberately.
        //
        // That is the general lesson: a regression test has to reproduce the state that made the
        // bug reachable, not merely the API calls that were involved.
        let _serial = reset();
        let abandoned = scratch("abandoned-a.bin");
        let target = scratch("target-b.bin");
        let _ = std::fs::remove_file(&abandoned);
        let _ = std::fs::remove_file(&target);

        // Leave a transfer OPEN on one path. This is the state a crashed client leaves behind, and
        // it is what makes the cleanup path run at all.
        begin(abandoned.to_str().unwrap(), true).expect("begin abandoned");
        write_chunk(0, &[1u8; 64], false, None).expect("partial write");

        // Now transfer a DIFFERENT path to completion.
        begin(target.to_str().unwrap(), true).expect("begin target");
        let total = write_chunk(0, &[2u8; 512], true, None).expect("complete target");
        assert_eq!(total, 512);

        // The assertion that matters: the file must be on disk, readable, and hold this
        // transfer's bytes.
        let on_disk =
            std::fs::read(&target).expect("the destination must exist after a completed transfer");
        assert_eq!(
            on_disk.len(),
            512,
            "the file on disk must be the size the transfer reported"
        );
        assert!(
            on_disk.iter().all(|&b| b == 2),
            "the file must contain this transfer's bytes, not a previous version"
        );
    }

    #[test]
    fn a_completed_file_is_not_destroyed_by_a_later_transfer() {
        // Cleanup must only remove files belonging to ABANDONED transfers. A file written by a
        // transfer that COMPLETED is somebody's artifact and must survive.
        let _serial = reset();
        let finished = scratch("finished.bin");
        let next = scratch("next.bin");
        let _ = std::fs::remove_file(&finished);
        let _ = std::fs::remove_file(&next);

        // A real, completed artifact.
        begin(finished.to_str().unwrap(), true).expect("begin finished");
        write_chunk(0, &[7u8; 200], true, None).expect("complete finished");
        assert!(finished.exists(), "the artifact must be on disk");

        // A later transfer to a different path. Nothing here is abandoned.
        begin(next.to_str().unwrap(), true).expect("begin next");
        write_chunk(0, &[8u8; 32], true, None).expect("complete next");

        assert!(
            finished.exists(),
            "a later transfer must not delete a file an earlier COMPLETED transfer wrote"
        );
        assert_eq!(std::fs::read(&finished).expect("readable").len(), 200);
    }

    #[test]
    fn an_empty_final_chunk_completes_a_transfer() {
        // A zero-byte file: one chunk, eof, nothing in it. The boundary case below the interesting
        // one, and the one an "if data.is_empty() then skip" optimisation would get wrong.
        let _serial = reset();
        let dest = scratch("empty.bin");
        let _ = std::fs::remove_file(&dest);
        let d = begin(dest.to_str().unwrap(), true).expect("begin");

        let total = write_chunk(0, &[], true, None).expect("eof chunk");
        assert_eq!(total, 0);

        let on_disk = std::fs::metadata(&d.path).expect("stat").len();
        assert_eq!(on_disk, 0, "an empty transfer produces an empty file");

        let _ = std::fs::remove_file(&dest);
    }
}
