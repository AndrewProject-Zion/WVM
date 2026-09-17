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
/// The read timeout for an ordinary QMP command.
///
/// QMP operations are request/response and a hung daemon must not hang the CLI forever. Jobs get a
/// longer timeout of their own — see `next_event_with_timeout`.
const DEFAULT_QMP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    /// Events read while waiting for a command reply, kept for `next_event` to drain.
    ///
    /// A `VecDeque` rather than a channel because there is a single reader and the ordering matters:
    /// job status transitions arrive in order and a caller inspecting them wants that order kept.
    pending_events: std::collections::VecDeque<Value>,
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
            .set_read_timeout(Some(DEFAULT_QMP_TIMEOUT))
            .context("setting a read timeout on the QMP socket")?;
        let reader = BufReader::new(stream.try_clone()?);

        let mut qmp = Qmp {
            stream,
            reader,
            pending_events: std::collections::VecDeque::new(),
        };

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
        self.execute_with_timeout(command, arguments, DEFAULT_QMP_TIMEOUT)
    }

    /// As `execute`, but with an explicit timeout for the COMMAND REPLY.
    ///
    /// A snapshot command does not merely start a job: while machine state is written, QEMU's main
    /// loop is blocked and the reply itself can take minutes. With the 10-second default the caller
    /// saw "Resource temporarily unavailable" and reported a FAILURE for a restore that had in fact
    /// succeeded — the marker file was gone and the guest was running normally. Only measuring the
    /// operation rather than the message exposed that.
    ///
    /// The timeout is set here and deliberately not left in place for anyone else: a quick command
    /// must not inherit a job's patience, and the next caller sets its own.
    pub fn execute_with_timeout(
        &mut self,
        command: &str,
        arguments: Option<Value>,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let _ = self.stream.set_read_timeout(Some(timeout));

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
        //
        // Events encountered here are QUEUED, not discarded. The first version threw them away,
        // which is wrong for any command whose completion is reported as events: `snapshot-save`
        // returns immediately and then reports progress through JOB_STATUS_CHANGE, and those
        // events can arrive BEFORE this reply. Discarding them meant the caller's event loop then
        // waited forever for a transition that had already been consumed — a race, so it appeared
        // intermittently and looked like a timeout rather than a lost message.
        //
        // The queue is drained by `next_event`.
        loop {
            let message = self.read_message()?;
            if let Some(err) = message.get("error") {
                bail!("QMP error for '{command}': {err}");
            }
            if message.get("return").is_some() {
                return Ok(message["return"].clone());
            }
            self.pending_events.push_back(message);
        }
    }

    /// Read the next asynchronous EVENT, or `None` if none arrives within `timeout`.
    ///
    /// `execute()` deliberately discards events while waiting for a command reply. Snapshot jobs
    /// are the opposite shape: the command returns immediately and the OUTCOME arrives as events,
    /// so something has to read them. That is this.
    ///
    /// A timeout returns `None` rather than an error, because waiting for an event that has not
    /// happened yet is the normal case — the caller loops. Distinguishing "nothing yet" from
    /// "something went wrong" is what stops a polling loop from treating ordinary patience as a
    /// failure.
    /// Read the next asynchronous EVENT, or `None` if the read timeout expires first.
    ///
    /// `execute()` deliberately discards events while waiting for a command reply. Snapshot jobs
    /// are the opposite shape: the command returns immediately and the OUTCOME arrives as events,
    /// so something has to read them. That is this.
    ///
    /// **This function never changes the socket timeout.** That is the whole design, and it is a
    /// correction of an earlier version that did — and broke the guest:
    ///
    /// - `connect()` already sets a read timeout for the connection, so there was nothing to add.
    /// - Overwriting it with a shorter value meant a slow-but-healthy read (a snapshot job taking
    ///   longer than two seconds) timed out as though it had hung.
    /// - Worse, restoring the timeout afterwards used `?`, so an error return skipped the restore
    ///   and left the short timeout in place for every later caller on a shared socket.
    /// - And `read_line` on a `BufReader` consumes bytes into its own buffer as it goes, so a
    ///   timeout part-way through a line left a fragment that the NEXT read resumed from. The
    ///   stream desynchronised permanently and QEMU was left mid-message, which wedged the guest
    ///   itself — not just the client.
    ///
    /// The lesson recorded here: mutating shared connection state to satisfy one caller is a
    /// trap, and the failure it produces (a dead control channel that looks like a hung guest) is
    /// the most expensive kind to diagnose in this project.
    ///
    /// A timeout returns `None` rather than an error, because an event that has not happened yet is
    /// the normal case — the caller loops until its own deadline.
    /// As `next_event`, with an explicit socket timeout for the read.
    ///
    /// Job events arrive over a job that legitimately runs for seconds, while the connection's
    /// default timeout is tuned for a quick command. So a job loop needs a longer one — and the
    /// restore of the default must happen on EVERY path.
    ///
    /// An earlier version of this function set a timeout, then restored it with `?`, so an error
    /// return skipped the restore and left a short timeout on a shared socket. Worse,
    /// `BufReader::read_line` had already consumed part of a message into its buffer, so the next
    /// read resumed mid-line, desynchronising the stream and wedging QEMU itself.
    ///
    /// The restore below is therefore unconditional and ignores its own error: there is no useful
    /// recovery if setting a socket option fails, and skipping it would reintroduce the wedge.
    pub fn next_event_with_timeout(
        &mut self,
        timeout: Option<std::time::Duration>,
    ) -> Result<Option<Value>> {
        // Anything already read while waiting for a command reply comes first. Without this, events
        // that arrived during `execute` would be invisible to the caller — which is exactly the
        // race that made snapshot-save look like it timed out.
        if let Some(queued) = self.pending_events.pop_front() {
            return Ok(Some(queued));
        }

        self.stream.set_read_timeout(timeout).ok();
        let result = self.read_message();
        // Restore to the CONNECTION default, on every path.
        //
        // The job loop sets a longer timeout per call, so the restore is what keeps a job's
        // generosity from leaking into the next quick command. It also cannot be skipped: an
        // earlier version restored it with `?` and a failed read left a short timeout on a shared
        // socket, which wedged the guest.
        //
        // Note the job loop passes its timeout on EVERY read rather than relying on this one
        // persisting. A 6.5-second pause while QEMU freezes the CPUs is normal during a snapshot,
        // so a single read can easily outlast the default — measured, not assumed.
        let _ = self.stream.set_read_timeout(Some(DEFAULT_QMP_TIMEOUT));

        match result {
            Ok(message) => {
                if message.get("event").is_some() {
                    Ok(Some(message))
                } else {
                    // A reply rather than an event: not what this call is for, and silently
                    // returning it would corrupt the caller's view of the stream.
                    Ok(None)
                }
            }
            Err(e) => {
                let text = e.to_string();
                if text.contains("timed out") || text.contains("WouldBlock") {
                    // Nothing yet. The caller's own deadline decides when to give up.
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Capture the guest's framebuffer, returning a PPM.
    ///
    /// QMP's `screendump` writes to a path rather than returning data, so this takes a destination
    /// and the caller reads it back.
    ///
    /// # Why the host captures rather than the guest
    ///
    /// The obvious design is for the guest service to screenshot itself, keeping every operation
    /// behind the grant check and the journal. That was the original plan and it cannot work: a
    /// Windows service runs in **session 0**, which has no interactive desktop, and `BitBlt` fails
    /// there because there is no screen to copy from. Verified on this guest —
    /// `query session` reports `services` at ID 0 and the logged-in user's `console` at ID 1.
    /// Session 0 isolation has been in Windows since Vista specifically to stop services touching
    /// the desktop, so this is the OS working as designed, not a bug to code around.
    ///
    /// Capturing on the host loses the guest as the actor, but keeps what actually mattered about
    /// running it in-guest: the operation still goes through the capability grant and the journal.
    /// The alternative — spawning a helper in session 1 via `WTSQueryUserToken` and
    /// `CreateProcessAsUser` — needs `SeTcbPrivilege` and per-request process creation, and is worth
    /// it only if the guest is not trusted to report truthfully about its own screen.
    ///
    /// `format` names the QMP format; omitting it lets QEMU choose, which is PPM on this build.
    pub fn screendump(&mut self, destination: &Path, format: Option<&str>) -> Result<()> {
        let mut args = json!({ "filename": destination.to_string_lossy() });
        if let Some(f) = format {
            args["format"] = json!(f);
        }

        // `screendump` is asynchronous when a format is given: it returns before the file is
        // written, so a caller reading immediately can see a partial image. Waiting on the
        // resulting event is the honest fix; the alternative is a sleep, which is a guess.
        self.execute("screendump", Some(args))?;

        if format.is_some() {
            self.wait_for_event("SCREENSHOT_COMPLETED", Duration::from_secs(30))?;
        }

        Ok(())
    }

    /// Block until an event with the given name arrives, or the timeout expires.
    fn wait_for_event(&mut self, name: &str, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let message = self.read_message()?;
            if let Some(event) = message.get("event") {
                if event.as_str() == Some(name) {
                    return Ok(());
                }
            }
        }
        bail!("timed out waiting for the QMP event '{name}'")
    }

    /// Send one key event, as an explicit press or release.
    ///
    /// `input-send-event` is used rather than the older `send-key` because it models press and
    /// release separately. `send-key` presses everything it is given and releases it again, so a
    /// chord like Ctrl+Alt+Delete arrives as four separate taps rather than a held combination,
    /// and a key cannot be held across a subsequent one at all.
    pub fn send_key_event(&mut self, event: &crate::input::KeyEvent) -> Result<()> {
        let mut events = Vec::new();

        if event.down {
            // Modifiers go down first, in the order given, and are released in reverse afterwards.
            for modifier in &event.modifiers {
                events.push(json!({
                    "type": "key",
                    "data": { "down": true, "key": { "type": "qcode", "data": modifier } }
                }));
            }
            events.push(json!({
                "type": "key",
                "data": { "down": true, "key": { "type": "qcode", "data": event.key } }
            }));
        } else {
            events.push(json!({
                "type": "key",
                "data": { "down": false, "key": { "type": "qcode", "data": event.key } }
            }));
            for modifier in event.modifiers.iter().rev() {
                events.push(json!({
                    "type": "key",
                    "data": { "down": false, "key": { "type": "qcode", "data": modifier } }
                }));
            }
        }

        self.execute("input-send-event", Some(json!({ "events": events })))?;
        Ok(())
    }

    /// Press a chord: every key but the last is held down across the final key.
    ///
    /// The modifier must be held ACROSS the keypress. Releasing it first — or sending the keys as
    /// separate presses — is read by Windows as the modifier key alone followed by an unrelated
    /// key, which is why an early attempt at Meta+R opened the Start menu instead of the Run box.
    pub fn send_chord(&mut self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            bail!("a chord needs at least one key");
        }

        let mut events = Vec::new();

        for modifier in &keys[..keys.len() - 1] {
            events.push(json!({
                "type": "key",
                "data": { "down": true, "key": { "type": "qcode", "data": modifier } }
            }));
        }

        let last = keys.last().expect("checked non-empty");
        events.push(json!({
            "type": "key",
            "data": { "down": true, "key": { "type": "qcode", "data": last } }
        }));
        events.push(json!({
            "type": "key",
            "data": { "down": false, "key": { "type": "qcode", "data": last } }
        }));

        for modifier in keys[..keys.len() - 1].iter().rev() {
            events.push(json!({
                "type": "key",
                "data": { "down": false, "key": { "type": "qcode", "data": modifier } }
            }));
        }

        self.execute("input-send-event", Some(json!({ "events": events })))?;
        Ok(())
    }

    /// Move the pointer to an absolute position.
    ///
    /// Uses the absolute axis, which requires a `usb-tablet` on the VM. Without one the guest has
    /// only a relative PS/2 mouse and a coordinate cannot be expressed — the position would be a
    /// displacement from wherever the pointer happens to be.
    pub fn send_pointer_move(&mut self, x: i32, y: i32) -> Result<()> {
        // A scaled axis takes a value in the range 0..=0x7fff mapped across the full width or
        // height, rather than a pixel coordinate. The guest's resolution is therefore not needed
        // here, which avoids the two getting out of step.
        const SCALE: i64 = 0x7fff;

        let (width, height) = self.query_screen_size()?;
        if width == 0 || height == 0 {
            bail!("the guest reports a {width}x{height} screen; cannot map a coordinate onto it");
        }

        let sx = (x as i64 * SCALE / width as i64).clamp(0, SCALE) as i32;
        let sy = (y as i64 * SCALE / height as i64).clamp(0, SCALE) as i32;

        let events = vec![
            json!({ "type": "abs", "data": { "axis": "x", "value": sx } }),
            json!({ "type": "abs", "data": { "axis": "y", "value": sy } }),
        ];
        self.execute("input-send-event", Some(json!({ "events": events })))?;

        // A short settle. The guest processes the axis events asynchronously, and a button event
        // sent in the same batch can be applied at the previous position — a click that lands
        // somewhere else, which is the worst kind of wrong because it usually succeeds.
        std::thread::sleep(Duration::from_millis(60));
        Ok(())
    }

    /// Press or release a mouse button.
    pub fn send_pointer_button(&mut self, button: &str, down: bool) -> Result<()> {
        let name = match button {
            "left" => "left",
            "right" => "right",
            "middle" => "middle",
            other => bail!("unknown mouse button {other:?}; use left, right or middle"),
        };

        let events = vec![json!({
            "type": "btn",
            "data": { "down": down, "button": name }
        })];
        self.execute("input-send-event", Some(json!({ "events": events })))?;
        Ok(())
    }

    /// Ask the guest what resolution it is running at.
    ///
    /// Read rather than assumed: mapping a coordinate onto the wrong dimensions puts the pointer
    /// somewhere valid-looking and wrong.
    ///
    /// # How, and why not a QMP query
    ///
    /// There is no reliable QMP command for this. `query-displays` does not exist on this QEMU
    /// (verified against `query-commands`), and the display device's geometry is not reachable
    /// through the QOM tree either — `/machine/graphics` reports `DeviceNotFound`.
    ///
    /// So this takes a screendump and reads the dimensions out of the PPM header. That is exact,
    /// it is the same framebuffer the pointer coordinates map onto, and it reuses a path that is
    /// already written and tested rather than adding a second, less reliable one.
    ///
    /// The cost is a screendump per absolute move, which for a 1024x768 guest is about 2 MB read
    /// from a temporary file and immediately discarded. That is worth paying for coordinates that
    /// are right: a click that lands at the wrong place usually still succeeds, which makes it the
    /// worst kind of wrong.
    pub fn query_screen_size(&mut self) -> Result<(u32, u32)> {
        let scratch = std::env::temp_dir().join(format!("wvm-geometry-{}.ppm", std::process::id()));

        self.screendump(&scratch, None)?;

        // Only the header is needed, but reading the file is simpler than a partial read and the
        // file is removed immediately either way.
        let result = crate::image::read_ppm(&scratch).map(|img| (img.width, img.height));
        let _ = std::fs::remove_file(&scratch);

        let (width, height) = result?;
        if width == 0 || height == 0 {
            bail!("the guest framebuffer reports {width}x{height}; a coordinate cannot be mapped");
        }

        Ok((width, height))
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
        // Prefer the pid file, which `wvm vm start` writes. Fall back to scanning for a QEMU
        // process belonging to this VM, because a VM started any other way — by
        // `scripts/vm-with-display.py`, or by hand — is running with no pid file at all.
        //
        // The fallback matters: deciding "stopped" from a missing file reported a live VM as
        // stopped, and `wvm vm capture` then refused to capture something that was plainly running.
        let pid = match self.read_pid() {
            Some(p) if process_is_alive(p) => p,
            _ => match self.find_running_pid() {
                Some(p) => p,
                None => return VmState::Stopped,
            },
        };

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

    /// Find a QEMU process for this VM by scanning the process table.
    ///
    /// The pid file is only written by `wvm vm start`, so a VM launched another way — by
    /// `scripts/vm-with-display.py`, or by hand — is running while the pid file is absent. Deciding
    /// "stopped" from a missing file was wrong in exactly that case: a live VM with a working QMP
    /// socket was reported stopped, and `wvm vm capture` refused to capture it.
    ///
    /// Matching is on the QEMU process name AND the VM's own name in its command line, so a second
    /// VM on the host cannot be mistaken for this one. `pgrep -f` is avoided deliberately: it
    /// matches the invoking shell's own command line, which has bitten this project before.
    pub fn find_running_pid(&self) -> Option<i32> {
        let entries = std::fs::read_dir("/proc").ok()?;

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };

            // Only consider processes whose executable is QEMU.
            let comm = std::fs::read_to_string(path.join("comm")).unwrap_or_default();
            if !comm.trim().starts_with("qemu-system") {
                continue;
            }

            // And whose command line names this VM, so another VM is not returned instead.
            //
            // `/proc/<pid>/cmdline` is NUL-separated, not space-separated. Reading it as a string
            // and searching for "-name w11" therefore never matches: the bytes are
            // `-name\0w11\0`, so the space is not there. Splitting on NUL first is what makes the
            // comparison meaningful, and checking the ARGUMENT rather than a joined string avoids
            // matching a path that happens to contain the same text.
            let cmdline = std::fs::read(path.join("cmdline")).unwrap_or_default();
            let args: Vec<String> = cmdline
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();

            let mut names_this_vm = false;
            for pair in args.windows(2) {
                if pair[0] == "-name" && pair[1] == self.config.name {
                    names_this_vm = true;
                    break;
                }
            }

            if names_this_vm {
                return Some(pid);
            }
        }

        None
    }

    /// Create the state directory and the disk image if they do not exist.
    pub fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(self.config.state_dir())
            .with_context(|| format!("creating {}", self.config.state_dir().display()))?;

        if !self.config.disk.exists() {
            // The disk's parent is NOT necessarily the state directory. An earlier version created
            // only the state directory and then failed with "No such file or directory" from
            // qemu-img whenever `disk` pointed somewhere else — which is the normal arrangement,
            // since the state directory holds runtime files and the disk is user data.
            if let Some(parent) = self.config.disk.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating the disk directory {}", parent.display()))?;
            }

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
        } else {
            // The disk exists, which means something may already be installed on it. If the
            // configured firmware does not match how it was installed, the guest will not boot and
            // the firmware will say nothing — an unbootable disk is a normal condition to it.
            //
            // This is worth a hard error rather than a warning: the symptom (a boot menu, or a
            // blank screen) gives no hint of the cause, and the fix is a one-line config change.
            if let Some(installed) = self.config.detect_installed_firmware() {
                if installed != self.config.firmware {
                    bail!(
                        "firmware mismatch: {} was installed under {:?} firmware but the config \
                         says {:?}.\n\
                         The guest will not boot, and the firmware will not report why.\n\
                         Fix: set `firmware = \"{}\"` in the VM config, or reinstall under the other \
                         firmware.",
                        self.config.disk.display(),
                        installed,
                        self.config.firmware,
                        match installed {
                            crate::vm::Firmware::Uefi => "uefi",
                            crate::vm::Firmware::Bios => "bios",
                        }
                    );
                }
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
            no_reboot: false,
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
    fn prepare_creates_the_disk_directory_when_it_differs_from_the_state_dir() {
        // Regression: the earlier version created only the state directory, then handed the disk
        // path to qemu-img, which failed with "No such file or directory" whenever the disk lived
        // somewhere else. That is the normal arrangement — state holds runtime files, the disk is
        // user data — so every earlier test missed it by co-locating the two.
        let root = scratch("prepare-separate");
        let state = root.join("state");
        let data = root.join("data");

        let c = VmConfig {
            name: "separate".into(),
            disk: data.join("disk.qcow2"),
            disk_gib: 1,
            memory_mib: 1024,
            cpus: 1,
            install_iso: None,
            driver_iso: None,
            firmware: Firmware::Bios,
            guest_port: 48273,
            forward_port: 48274,
            state_dir: Some(state.clone()),
            no_reboot: false,
        };

        let s = Supervisor::new(c.clone());
        s.prepare()
            .expect("prepare must create the disk's parent directory");

        assert!(state.exists(), "the state directory should exist");
        assert!(data.exists(), "the disk's directory should exist");
        assert!(c.disk.exists(), "the disk image should have been created");
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
