//! Snapshot and restore, driven from the host over QMP.
//!
//! # Why this is host-side
//!
//! For the same reason as capture and input (D-009, D-010): the operation belongs to the
//! hypervisor. The guest is a VM with no view of its own QEMU process, and the guest service runs
//! in session 0 with no desktop. Snapshotting is a QEMU capability, so it is driven from the side
//! that owns QEMU.
//!
//! # What a snapshot actually is here
//!
//! `snapshot-save` writes the machine state — RAM plus device state — INTO the disk's own qcow2
//! file as an internal snapshot. There is no separate vmstate file, and this was the single most
//! expensive misunderstanding in the project: the QAPI documentation carries a worked example where
//! `vmstate` is the *disk node itself*:
//!
//! ```text
//! { "execute": "snapshot-save",
//!   "arguments": { "job-id": "snapsave0", "tag": "my-snap",
//!                  "vmstate": "disk0", "devices": ["disk0"] } }
//! ```
//!
//! Inventing a separate vmstate block device produces `vmstate block device 'X' does not exist` —
//! for a node that `query-named-block-nodes` lists as present and writable. That contradiction is
//! the tell that the parameter means something other than its name suggests. See D-016.
//!
//! # The job API, and why the events are not enough
//!
//! `snapshot-save` and `snapshot-load` return immediately. Completion arrives as
//! `JOB_STATUS_CHANGE` events: `created -> running -> waiting -> pending -> concluded`. **None of
//! those events carries the reason for a failure.** The error is only visible from `query-jobs`.
//!
//! A caller that watched the event stream alone would see a job abort with no explanation, which is
//! exactly what happened during development: four attempts at a job that reported
//! `aborting -> concluded` and said nothing about why. So this module always consults `query-jobs`
//! after the job concludes, and surfaces the error it finds.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::supervisor::Qmp;

/// The block node the VM state goes into, and the node snapshotted.
///
/// This is the disk's FORMAT node, named in `vm.rs` when the disk is declared with `-blockdev`.
/// It has to be an explicit name: `-drive` produces an anonymous node (`#block172`) and
/// `snapshot-save` refuses those outright, which is why the disk declaration changed (D-016).
pub const DISK_NODE: &str = "disk0";

/// How long to wait for a snapshot job before giving up on it.
///
/// Writing 3-4 GiB of machine state through a qcow2 takes seconds on a warm page cache and longer
/// on a cold one. Generous, because a snapshot that is abandoned halfway is worse than one that
/// reports a timeout.
const SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How long a single read may wait for the next job event.
///
/// Snapshots of a running guest take seconds, and there can be a pause between status transitions
/// while QEMU flushes. This is per-read, not for the whole job: the overall bound is
/// `SNAPSHOT_TIMEOUT`.
const JOB_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A counter for job ids, so repeated snapshots under one tag do not collide.
///
/// QEMU keeps concluded jobs and refuses a reused id. Combined with the process id this is unique
/// for any single host invocation, which is as long as a job can live.
fn next_job_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    // The pid keeps two concurrent invocations from sharing an id.
    (u64::from(std::process::id()) << 16) | (n & 0xffff)
}

/// A snapshot on a disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub tag: String,
    pub vm_size_bytes: u64,
}

/// Wait for a QMP job to conclude, then report its error if it had one.
///
/// The two halves matter equally. Waiting is obvious. **Reading `query-jobs` is the part that is
/// easy to omit and impossible to work without** — the status events say a job ended, never why.
fn await_job(qmp: &mut Qmp, job_id: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + SNAPSHOT_TIMEOUT;

    loop {
        if std::time::Instant::now() > deadline {
            bail!("the snapshot job '{job_id}' did not conclude within {SNAPSHOT_TIMEOUT:?}");
        }

        // A job legitimately runs for seconds while the connection's default read timeout is
        // tuned for a quick command, so the job loop asks for a longer one. It is restored by
        // `next_event_with_timeout` on every path, including the timeout itself.
        let event = qmp.next_event_with_timeout(Some(JOB_READ_TIMEOUT))?;
        let Some(event) = event else {
            // The read timed out. Not an error: the deadline at the top of the loop decides when to
            // give up. The read itself blocks for its timeout, so this does not spin.
            continue;
        };

        if event.get("event").and_then(Value::as_str) != Some("JOB_STATUS_CHANGE") {
            continue;
        }
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        if data.get("id").and_then(Value::as_str) != Some(job_id) {
            continue;
        }
        if data.get("status").and_then(Value::as_str) != Some("concluded") {
            continue;
        }
        break;
    }

    // The job concluded. That says NOTHING about whether it succeeded — the events do not carry
    // errors, so the outcome has to be fetched separately. Skipping this is how a failed snapshot
    // reports as a successful one.
    let jobs = qmp.execute("query-jobs", None).context("querying jobs")?;
    let jobs = jobs.as_array().cloned().unwrap_or_default();
    let job = jobs
        .iter()
        .find(|j| j.get("id").and_then(Value::as_str) == Some(job_id))
        .with_context(|| format!("job '{job_id}' concluded but is not in query-jobs"))?;

    if let Some(err) = job.get("error").and_then(Value::as_str) {
        if !err.is_empty() {
            bail!("snapshot job '{job_id}' failed: {err}");
        }
    }
    Ok(())
}

/// Save the machine state into an internal snapshot of the disk.
pub fn save(qmp: &mut Qmp, tag: &str) -> Result<Snapshot> {
    validate_tag(tag)?;

    // Replace an existing snapshot rather than failing on it.
    //
    // QEMU refuses a duplicate tag: "Snapshot 'proof' already exists in one or more devices". That
    // is the right default for a bare API, and the wrong behaviour for this one — a caller
    // snapshotting "before-install" twice wants the newer state, not an error telling them they
    // already have one. Re-saving under a name is how you refresh it.
    //
    // Deleting first also means the name is free even if the previous save left it inconsistent,
    // which is a state a half-written snapshot can reach.
    match delete(qmp, tag) {
        Ok(()) => {}
        Err(_) => {
            // No previous snapshot, which is the normal case. Nothing to report: the failure mode
            // being swallowed here is "there was nothing to delete".
        }
    }

    // The job id must be UNIQUE, while the tag may repeat.
    //
    // A tag is a name and re-saving under the same name is legitimate — it replaces the snapshot.
    // A job id is a handle, and QEMU keeps concluded jobs in `query-jobs` and rejects a reuse with
    // "Job ID '...' already exists". Deriving the id from the tag made every second save under the
    // same name fail, which the rollback probe caught on its second run.
    //
    // The process id and a monotonic counter are enough: jobs live for one host invocation, so
    // there is nothing to collide with across processes.
    let job_id = format!("wvm-save-{tag}-{}", next_job_seq());
    qmp.execute(
        "snapshot-save",
        Some(serde_json::json!({
            "job-id": job_id,
            // The vmstate node IS the disk node. See the module comment: this is not a separate
            // device, and treating it as one produces an error that describes a present node as
            // missing.
            "vmstate": DISK_NODE,
            "devices": [DISK_NODE],
            "tag": tag,
        })),
    )
    .context("starting snapshot-save")?;

    await_job(qmp, &job_id)?;

    // Confirm it exists rather than trusting the job. "The job reported success" and "the snapshot
    // is on the disk" are different claims, and only the second is what the caller asked for.
    let snapshots = list(qmp)?;
    snapshots
        .into_iter()
        .find(|s| s.tag == tag)
        .with_context(|| format!("snapshot-save reported success but '{tag}' is not on the disk"))
}

/// Restore the machine to an internal snapshot of the disk.
///
/// The guest is running throughout; this is a live restore, which is the whole reason the disk is
/// declared with `-blockdev` rather than the simpler `-drive` (D-016).
pub fn restore(qmp: &mut Qmp, tag: &str) -> Result<()> {
    validate_tag(tag)?;

    // Unique for the same reason as `save` — see the note there.
    let job_id = format!("wvm-load-{tag}-{}", next_job_seq());
    qmp.execute(
        "snapshot-load",
        Some(serde_json::json!({
            "job-id": job_id,
            "vmstate": DISK_NODE,
            "devices": [DISK_NODE],
            "tag": tag,
        })),
    )
    .context("starting snapshot-load")?;

    await_job(qmp, &job_id)
}

/// Delete an internal snapshot from the disk.
///
/// Synchronous, unlike save and load: no job, no events, and no `query-jobs` step. It is also the
/// only one of the three reachable from a simpler command name.
pub fn delete(qmp: &mut Qmp, tag: &str) -> Result<()> {
    validate_tag(tag)?;
    qmp.execute(
        "blockdev-snapshot-delete-internal-sync",
        Some(serde_json::json!({ "device": DISK_NODE, "name": tag })),
    )
    .with_context(|| format!("deleting snapshot '{tag}'"))?;
    Ok(())
}

/// Every snapshot on the disk, as the guest's own view of them.
///
/// Read through the human monitor because QMP has no direct query for internal snapshots. The
/// output is a table, so this parses it rather than inventing a shape QEMU does not provide.
pub fn list(qmp: &mut Qmp) -> Result<Vec<Snapshot>> {
    let raw = qmp
        .execute(
            "human-monitor-command",
            Some(serde_json::json!({ "command-line": "info snapshots" })),
        )
        .context("reading the snapshot list")?;
    parse_snapshot_table(raw.as_str().unwrap_or(""))
}

/// Parse the table `info snapshots` prints.
///
/// Split out from `list` so it can be tested against real output without a live hypervisor. That
/// separation matters more than usual here: the first version of this parser was tested only
/// through a sample written from assumption, and it silently returned nothing for every real row.
/// See the test for the full history.
///
/// The rows look like this, and the ID column is a **dash**, not a number:
///
/// ```text
/// ID      TAG          VM_SIZE      DATE                 VM_CLOCK     ICOUNT
/// --      my-snap      3.37 GiB  2026-09-16 12:57:50  0000:08:49.636   --
/// ```
///
/// An unparseable line is skipped rather than failing the whole call, because the output contains
/// headings and because a future QEMU may add columns. What must NOT happen is a silent empty
/// result for a table that plainly has rows — which is why the row detection is on shape rather
/// than on a field that looked like an identifier.
fn parse_snapshot_table(text: &str) -> Result<Vec<Snapshot>> {
    let mut out = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of") {
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        // id, tag, size, unit at minimum.
        if fields.len() < 4 {
            continue;
        }
        // The header row says VM_SIZE where a number would be.
        if fields[2] == "VM_SIZE" {
            continue;
        }

        let (id, tag, size, unit) = (fields[0], fields[1], fields[2], fields[3]);

        // A size that parses is the strongest signal this is a data row: headings and separators
        // do not carry "3.37 GiB".
        let Ok(vm_size_bytes) = parse_size(size, unit) else {
            continue;
        };

        out.push(Snapshot {
            id: id.to_string(),
            tag: tag.to_string(),
            vm_size_bytes,
        });
    }

    Ok(out)
}

/// Turn QEMU's two-field size ("3.37 GiB") into bytes.
fn parse_size(value: &str, unit: &str) -> Result<u64> {
    let n: f64 = value.parse().with_context(|| format!("size {value:?}"))?;
    let multiplier: f64 = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => bail!("unknown size unit {other:?}"),
    };
    Ok((n * multiplier) as u64)
}

/// Reject a tag QEMU would accept but a caller would regret.
///
/// The tag ends up in a monitor command line and in a snapshot name, so a tag containing
/// whitespace or a quote could do more than name a snapshot. Refusing at the boundary keeps the
/// failure here rather than inside the monitor.
fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() {
        bail!("a snapshot needs a name");
    }
    if tag.len() > 64 {
        bail!("snapshot names are limited to 64 characters");
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("snapshot names may contain letters, digits, dash, underscore and dot: {tag:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_with_whitespace_is_refused() {
        // It would end up inside a monitor command line.
        assert!(validate_tag("has space").is_err());
        assert!(validate_tag("has\ttab").is_err());
    }

    #[test]
    fn a_tag_that_is_empty_or_absurdly_long_is_refused() {
        assert!(validate_tag("").is_err());
        assert!(validate_tag(&"x".repeat(65)).is_err());
        assert!(validate_tag(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn reasonable_tags_are_accepted() {
        for tag in ["snap-1", "before_install", "v1.2.3", "abc123"] {
            assert!(validate_tag(tag).is_ok(), "{tag} should be allowed");
        }
    }

    #[test]
    fn a_size_in_gib_converts_to_bytes() {
        // 3.37 * 1024^3 = 3_618_509_946. An earlier version of this test asserted a value
        // computed in my head and failed against correct code — the reverse of the usual
        // mistake, and a reminder that the expectation deserves the same scepticism as the
        // implementation.
        assert_eq!(parse_size("3.37", "GiB").unwrap(), 3_618_509_946);
        assert_eq!(parse_size("0", "B").unwrap(), 0);
        assert_eq!(parse_size("512", "MiB").unwrap(), 536_870_912);
    }

    #[test]
    fn an_unknown_size_unit_is_an_error_rather_than_a_guess() {
        // Silently treating an unknown unit as bytes would under-report by a factor of a billion,
        // and the number is shown to a caller deciding whether a snapshot is worth keeping.
        assert!(parse_size("1", "PB").is_err());
    }

    #[test]
    fn the_snapshot_table_parses_the_shape_qemu_prints() {
        // VERBATIM output from `info snapshots` on this machine, obtained by
        // `scripts/probe-snapshot-output.py` rather than written from memory.
        //
        // That distinction is the whole point of this test's history. The first version used a
        // sample where the ID column was a NUMBER, invented from what the column is called. QEMU
        // renders it as `--` for internal snapshots, so the parser — which required a digit —
        // skipped every real row and reported "no snapshots" while two existed. The test passed,
        // because the test was written against the same wrong assumption as the code.
        //
        // A parser test is only worth anything if its input is real. See D-016.
        let raw = "List of snapshots present on all disks:\r\n\
                   ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\r\n\
                   --      my-snap          3.37 GiB 2026-09-16 12:57:50  0000:08:49.636         --\r\n\
                   --      before-test      2.76 GiB 2026-09-16 13:14:23  0000:01:04.552         --\r\n";

        let snapshots = parse_snapshot_table(raw).expect("the real table must parse");

        assert_eq!(
            snapshots.len(),
            2,
            "both data rows must be found, not skipped: {snapshots:?}"
        );
        assert_eq!(snapshots[0].tag, "my-snap");
        assert_eq!(snapshots[1].tag, "before-test");
        assert_eq!(
            snapshots[0].id, "--",
            "the id column is a dash, not a number"
        );
        assert_eq!(snapshots[0].vm_size_bytes, 3_618_509_946);
    }

    #[test]
    fn the_header_and_headings_are_not_mistaken_for_rows() {
        // The failure mode this guards is an empty list, which is also a legitimate answer — so a
        // broken parser is indistinguishable from a disk with no snapshots unless the row detection
        // is actually tested.
        let headers = "List of snapshots present on all disks:\r\n\
                       ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\r\n";
        assert_eq!(parse_snapshot_table(headers).unwrap().len(), 0);

        let empty = "List of snapshots present on all disks:\r\n";
        assert_eq!(parse_snapshot_table(empty).unwrap().len(), 0);
    }
}
