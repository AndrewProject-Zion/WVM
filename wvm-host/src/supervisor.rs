//! QEMU process supervision.
//!
//! Lifecycle is driven over **QMP** (the QEMU Machine Protocol) rather than signals. `SIGTERM`
//! to a VM gives you a process that stopped; QMP gives you a guest that shut down, a snapshot
//! that was taken, or a suspend that actually suspended. The difference matters when the thing
//! being controlled holds a filesystem.
//!
//! ## Liveness is decided by the PID file plus a liveness check, never by the file alone
//!
//! A PID file outlives the process that wrote it, and — worse — PIDs are recycled. A stale file
//! naming a PID that now belongs to something else is indistinguishable from a running VM if you
//! only check that the file exists. So: read the PID, ask the OS whether that process is alive,
//! and only then believe it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::vm::VmConfig;

/// A one-shot QMP conversation.
///
/// QMP is line-delimited JSON: the server sends a greeting, the client sends
/// `qmp_capabilities`, then commands. Implemented directly rather than through a client crate so
/// the wire format is visible and the dependency list stays short — this is a small enough
/// protocol that a library would be more surface area than help.
pub struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl std::fmt::Debug for Qmp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The streams are not usefully printable and a `UnixStream` has no Debug of its own here.
        f.debug_struct("Qmp").field("connected", &true).finish()
    }
}

impl Qmp {
    /// Connect and negotiate capabilities.
    pub fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to QMP socket {}", socket.display()))?;
        // QMP operations are request/response; a hung daemon must not hang the CLI forever.
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .context("setting a read timeout on the QMP socket")?;
        let reader = BufReader::new(stream.try_clone()?);

        let mut qmp = Qmp { stream, reader };

        // The greeting arrives unprompted and must be consumed before anything else.
        let greeting = qmp.read_message()?;
        if greeting.get("QMP").is_none() {
            bail!("unexpected QMP greeting: {greeting}");
        }

        qmp.execute("qmp_capabilities", None)?;
        Ok(qmp)
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .context("reading from the QMP socket")?;
        if line.trim().is_empty() {
            bail!("QMP closed the connection");
        }
        serde_json::from_str(&line).with_context(|| format!("parsing a QMP message: {line}"))
    }

    /// Run one command and return its result.
    pub fn execute(&mut self, command: &str, arguments: Option<Value>) -> Result<Value> {
        let mut payload = json!({ "execute": command });
        if let Some(args) = arguments {
            payload["arguments"] = args;
        }
        let text = serde_json::to_string(&payload)?;
        self.stream
            .write_all(text.as_bytes())
            .context("writing a QMP command")?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;

        // Skip asynchronous events until the reply carrying our command's result arrives.
        loop {
            let message = self.read_message()?;
            if let Some(err) = message.get("error") {
                bail!("QMP error for '{command}': {err}");
            }
            if message.get("return").is_some() {
                return Ok(message["return"].clone());
            }
            // Anything else is an event; ignore it here.
        }
    }
}

/// Current state of a VM, as observed rather than assumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmState {
    /// No PID file, or the PID in it is not a live process.
    Stopped,
    /// A process is running and QMP answers; carries QEMU's own status string.
    Running { status: String },
    /// A process is running but QMP does not answer. Worth distinguishing: this is the state
    /// that looks fine and is not.
    Unresponsive { pid: i32 },
}

impl VmState {
    pub fn is_running(&self) -> bool {
        !matches!(self, VmState::Stopped)
    }

    pub fn label(&self) -> String {
        match self {
            VmState::Stopped => "stopped".into(),
            VmState::Running { status } => format!("running ({status})"),
            VmState::Unresponsive { pid } => format!("running but unresponsive (pid {pid})"),
        }
    }
}

/// Supervisor for one VM definition.
pub struct Supervisor {
    config: VmConfig,
}

impl Supervisor {
    pub fn new(config: VmConfig) -> Self {
        Supervisor { config }
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    /// Determine the current state without changing anything.
    pub fn state(&self) -> VmState {
        let pid = match self.read_pid() {
            Some(p) => p,
            None => return VmState::Stopped,
        };

        if !process_is_alive(pid) {
            // The file outlived the process. Report Stopped rather than trusting the file.
            return VmState::Stopped;
        }

        match Qmp::connect(&self.config.qmp_socket()) {
            Ok(mut qmp) => match qmp.execute("query-status", None) {
                Ok(value) => {
                    let status = value
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    VmState::Running { status }
                }
                Err(_) => VmState::Unresponsive { pid },
            },
            Err(_) => VmState::Unresponsive { pid },
        }
    }

    fn read_pid(&self) -> Option<i32> {
        let text = std::fs::read_to_string(self.config.pid_file()).ok()?;
        text.trim().parse::<i32>().ok()
    }

    /// Create the state directory and the disk image if they do not exist.
    pub fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(self.config.state_dir())
            .with_context(|| format!("creating {}", self.config.state_dir().display()))?;

        if !self.config.disk.exists() {
            let size = format!("{}G", self.config.disk_gib);
            let status = Command::new("qemu-img")
                .args(["create", "-f", "qcow2"])
                .arg(&self.config.disk)
                .arg(&size)
                .status()
                .context("running qemu-img (is QEMU installed?)")?;
            if !status.success() {
                bail!("qemu-img failed to create {}", self.config.disk.display());
            }
        }

        // A writable copy of the UEFI variable store is required per instance; without it,
        // NVRAM settings are shared between VMs or lost entirely.
        if self.config.firmware == crate::vm::Firmware::Uefi {
            let vars = self.config.state_dir().join("OVMF_VARS.fd");
            if !vars.exists() {
                let src = "/usr/share/OVMF/OVMF_VARS_4M.fd";
                std::fs::copy(src, &vars).with_context(|| {
                    format!("copying UEFI variable store from {src} (is ovmf installed?)")
                })?;
            }
        }

        Ok(())
    }

    /// Start the VM, detached. Returns the PID.
    pub fn start(&self) -> Result<i32> {
        let current = self.state();
        if current.is_running() {
            bail!("VM '{}' is already {}", self.config.name, current.label());
        }

        self.prepare()?;

        // Remove a stale PID file before launching, so a failure to start cannot be mistaken for
        // a successful start using an old PID.
        let _ = std::fs::remove_file(self.config.pid_file());

        let args = self.config.qemu_args();
        let child = spawn_detached("qemu-system-x86_64", &args, &self.config)?;
        let pid = child.id() as i32;

        std::fs::write(self.config.pid_file(), pid.to_string())
            .with_context(|| format!("writing {}", self.config.pid_file().display()))?;

        Ok(pid)
    }

    /// Wait until QMP answers, or give up.
    ///
    /// Returning as soon as the process spawns is not the same as the VM being up: QEMU takes
    /// time to initialise KVM and firmware. Reporting "started" before QMP answers is the kind
    /// of claim that makes a supervisor untrustworthy.
    pub fn wait_until_responsive(&self, timeout: Duration) -> Result<VmState> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.state();
            if matches!(state, VmState::Running { .. }) {
                return Ok(state);
            }
            if Instant::now() >= deadline {
                return Ok(state);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Suspend the guest to the disk image, returning resources to the host.
    ///
    /// `snapshot-save` is the **QMP** form. The familiar `savevm` is an HMP-only command and
    /// returns `CommandNotFound` over QMP — a mistake that only surfaces by running it against a
    /// live QEMU, which is how this was found.
    ///
    /// The state is written into the qcow2 image itself, so a suspend is durable without a
    /// separate state file to keep track of.
    pub fn suspend(&self) -> Result<()> {
        // Device names are read from QEMU rather than hardcoded: `snapshot-save` wants the
        // node names as QEMU knows them, and assuming "disk0" would break the moment the
        // command line changes.
        let device = self.image_node_name()?;
        let vmstate = match &device {
            Some(name) => name.clone(),
            None => bail!(
                "cannot suspend: no writable disk device found to hold the VM state \
                 (a guest with no disk has nowhere to save to)"
            ),
        };

        self.qmp_command(
            "snapshot-save",
            Some(json!({
                "job-id": "wvm-suspend",
                "tag": "wvm-suspend",
                "vmstate": vmstate,
                "devices": [device.unwrap_or_default()],
            })),
        )
        .context("suspending the VM")?;

        Ok(())
    }

    /// Find the block node name of the main disk, as QEMU reports it.
    ///
    /// Returns `None` rather than guessing when nothing suitable is present.
    fn image_node_name(&self) -> Result<Option<String>> {
        let mut qmp = Qmp::connect(&self.config.qmp_socket())
            .context("connecting to the running VM (is it started?)")?;
        let result = qmp.execute("query-block", None)?;

        let blocks = match result.as_array() {
            Some(b) => b,
            None => return Ok(None),
        };

        for entry in blocks {
            // Only nodes that are actually inserted and writable can hold VM state.
            let inserted = match entry.get("inserted") {
                Some(i) if !i.is_null() => i,
                _ => continue,
            };
            let readonly = inserted
                .get("ro")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if readonly {
                continue;
            }
            // Skip the firmware flash devices: they are not where a snapshot belongs.
            if let Some(device) = entry.get("device").and_then(|d| d.as_str()) {
                if device.starts_with("pflash") {
                    continue;
                }
            }
            // Prefer the explicit node name, falling back to the device name.
            if let Some(node) = inserted.get("node-name").and_then(|n| n.as_str()) {
                return Ok(Some(node.to_string()));
            }
            if let Some(device) = entry.get("device").and_then(|d| d.as_str()) {
                return Ok(Some(device.to_string()));
            }
        }

        Ok(None)
    }

    /// Shut the guest down cleanly, waiting for it to go.
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        // `system_powerdown` asks the guest to shut down — the ACPI equivalent of pressing the
        // power button. If the guest ignores it, the caller can escalate to kill.
        self.qmp_command("system_powerdown", None)
            .context("requesting a clean shutdown")?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.state().is_running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        bail!(
            "the guest did not shut down within {}s; it may be ignoring ACPI (use `wvm vm kill` to force it)",
            timeout.as_secs()
        )
    }

    fn qmp_command(&self, command: &str, args: Option<Value>) -> Result<Value> {
        let mut qmp = Qmp::connect(&self.config.qmp_socket())
            .context("connecting to the running VM (is it started?)")?;
        qmp.execute(command, args)
    }

    /// Read the tail of the serial log: the first place to look when a VM does not come up.
    pub fn serial_tail(&self, lines: usize) -> Result<String> {
        let text = std::fs::read_to_string(self.config.serial_log())
            .with_context(|| format!("reading {}", self.config.serial_log().display()))?;
        let collected: Vec<&str> = text.lines().collect();
        let start = collected.len().saturating_sub(lines);
        Ok(collected[start..].join("\n"))
    }
}

/// Is this PID a live process we could actually signal?
fn process_is_alive(pid: i32) -> bool {
    // `kill(pid, 0)` performs the permission and existence checks without sending a signal —
    // the standard way to ask "is this alive?" without a dependency on /proc internals.
    unsafe {
        // SAFETY: signal 0 sends nothing; it only performs error checking.
        libc_kill(pid, 0) == 0
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Spawn QEMU detached from this process.
///
/// QEMU must outlive the CLI invocation that started it, so its stdio is redirected to files in
/// the state directory rather than inherited. `setsid` would be ideal, but a short-lived parent
/// exiting already reparents the child; the important part is that nothing here holds the child
/// open.
fn spawn_detached(program: &str, args: &[String], config: &VmConfig) -> Result<Child> {
    let log = std::fs::File::create(config.monitor_log())
        .with_context(|| format!("creating {}", config.monitor_log().display()))?;
    let log_err = log.try_clone()?;

    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .with_context(|| format!("spawning {program} (is QEMU installed?)"))?;

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::Firmware;
    use std::path::PathBuf;

    fn config(dir: &str) -> VmConfig {
        VmConfig {
            name: "test-vm".into(),
            disk: PathBuf::from(dir).join("disk.qcow2"),
            disk_gib: 1,
            memory_mib: 1024,
            cpus: 1,
            install_iso: None,
            driver_iso: None,
            firmware: Firmware::Bios, // avoid needing OVMF in tests
            guest_port: 48273,
            forward_port: 48274,
            state_dir: Some(PathBuf::from(dir)),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("wvm-supervisor-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn no_pid_file_means_stopped() {
        let dir = scratch("stopped");
        let s = Supervisor::new(config(&dir.display().to_string()));
        assert_eq!(s.state(), VmState::Stopped);
    }

    #[test]
    fn a_stale_pid_file_naming_a_dead_process_means_stopped() {
        // The core trap: the file exists, so a naive check says "running".
        let dir = scratch("stale");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());

        std::fs::create_dir_all(c.state_dir()).unwrap();
        // A PID high enough that it is virtually certain not to be in use.
        std::fs::write(c.pid_file(), "4194303").unwrap();

        assert_eq!(
            s.state(),
            VmState::Stopped,
            "a PID file whose process is gone must report Stopped, not Running"
        );
    }

    #[test]
    fn a_pid_file_naming_this_live_process_is_not_reported_as_stopped() {
        // The converse guard: our own PID is definitely alive, so the state must NOT be Stopped.
        // It will be Unresponsive (nothing is listening on the QMP socket), which is exactly the
        // distinction the enum exists to make.
        let dir = scratch("live");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());

        std::fs::create_dir_all(c.state_dir()).unwrap();
        std::fs::write(c.pid_file(), std::process::id().to_string()).unwrap();

        match s.state() {
            VmState::Unresponsive { pid } => assert_eq!(pid, std::process::id() as i32),
            other => panic!("expected Unresponsive, got {other:?}"),
        }
    }

    #[test]
    fn unresponsive_is_reported_distinctly_from_stopped() {
        // "Running but not answering" and "not running" are different problems with different
        // fixes. Collapsing them is how a supervisor starts giving bad advice.
        let dir = scratch("unresponsive");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());
        std::fs::create_dir_all(c.state_dir()).unwrap();
        std::fs::write(c.pid_file(), std::process::id().to_string()).unwrap();

        let state = s.state();
        assert!(state.is_running(), "a live PID counts as running");
        assert!(!matches!(state, VmState::Stopped));
        assert!(
            state.label().contains("unresponsive"),
            "label: {}",
            state.label()
        );
    }

    #[test]
    fn prepare_creates_the_state_directory() {
        let dir = scratch("prepare");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());
        s.prepare()
            .expect("prepare should succeed with qemu-img present");
        assert!(c.state_dir().exists());
    }

    #[test]
    fn start_refuses_when_already_running() {
        let dir = scratch("double-start");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());
        std::fs::create_dir_all(c.state_dir()).unwrap();
        // Our own PID, alive, so the supervisor believes a VM is running.
        std::fs::write(c.pid_file(), std::process::id().to_string()).unwrap();

        let err = s.start().expect_err("must refuse a second start");
        assert!(
            err.to_string().contains("already"),
            "the error should say it is already running: {err}"
        );
    }

    #[test]
    fn serial_tail_reads_the_last_lines() {
        let dir = scratch("serial");
        let c = config(&dir.display().to_string());
        let s = Supervisor::new(c.clone());
        std::fs::create_dir_all(c.state_dir()).unwrap();
        std::fs::write(c.serial_log(), "one\ntwo\nthree\nfour\n").unwrap();

        assert_eq!(s.serial_tail(2).unwrap(), "three\nfour");
        assert_eq!(s.serial_tail(100).unwrap(), "one\ntwo\nthree\nfour");
    }

    #[test]
    fn qmp_connect_fails_cleanly_with_no_socket() {
        let missing = Path::new("/tmp/wvm-no-such-qmp-socket.sock");
        let _ = std::fs::remove_file(missing);
        let err = Qmp::connect(missing).expect_err("must fail, not panic");
        assert!(
            err.to_string().contains("connecting to QMP"),
            "error should name the operation: {err}"
        );
    }
}
