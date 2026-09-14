//! VM definition: what a WVM instance actually is, on disk.
//!
//! Kept as a declarative file rather than baked into the binary, so a user can inspect and edit
//! what will run without recompiling — and so the generated QEMU command line is reproducible
//! from something readable.
//!
//! Format: TOML. See `docs/VM-CONFIG.md` for the annotated reference.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// A VM definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmConfig {
    /// Display name, used in the journal and for the QEMU `-name` argument.
    pub name: String,

    /// Path to the Windows disk image. Created if absent when `create` is called.
    pub disk: PathBuf,

    /// Disk size to create, in GiB, when the image does not yet exist.
    #[serde(default = "default_disk_gib")]
    pub disk_gib: u64,

    /// Memory in MiB.
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,

    /// vCPUs.
    #[serde(default = "default_cpus")]
    pub cpus: u8,

    /// Path to an installer ISO. Present during install; absent for normal use.
    #[serde(default)]
    pub install_iso: Option<PathBuf>,

    /// Path to the virtio-win driver ISO, attached as a second CD-ROM.
    ///
    /// Windows has no in-box virtio drivers, so without this the guest cannot see its own disk.
    /// Attached read-only and never bootable.
    #[serde(default)]
    pub driver_iso: Option<PathBuf>,

    /// Firmware. UEFI is the default because it is what Windows 11 expects; tiny11 tolerates
    /// legacy BIOS but there is no reason to choose it.
    #[serde(default)]
    pub firmware: Firmware,

    /// The transport the guest service listens on, as seen from the host.
    ///
    /// This is a host-side port forward to the guest's loopback, not a bridged interface: the
    /// guest should not be reachable from the network, only from this host.
    #[serde(default = "default_guest_port")]
    pub guest_port: u16,

    /// Host port forwarded to the guest's control port.
    #[serde(default = "default_forward_port")]
    pub forward_port: u16,

    /// Where runtime state lives: PID file, QMP socket, serial log. Defaults to a directory
    /// derived from the instance name.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,

    /// Exit QEMU when the guest reboots, instead of rebooting it.
    ///
    /// Defaults to `false`, which is the correct setting for installation: Windows reboots
    /// several times during setup, and exiting on the first of those makes the install appear to
    /// vanish. Set `true` for a long-lived VM where a silent guest reboot should leave a visible
    /// stopped VM rather than an empty process.
    #[serde(default)]
    pub no_reboot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    #[default]
    Uefi,
    Bios,
}

fn default_disk_gib() -> u64 {
    64
}

fn default_memory_mib() -> u64 {
    4096
}

fn default_cpus() -> u8 {
    4
}

fn default_guest_port() -> u16 {
    48273
}

fn default_forward_port() -> u16 {
    48274
}

impl VmConfig {
    /// Load from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading VM config {}", path.display()))?;
        let config: VmConfig = toml::from_str(&text)
            .with_context(|| format!("parsing VM config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Check the definition is internally consistent and the files it names exist.
    ///
    /// Run before anything is launched. Discovering a missing disk image after QEMU has already
    /// started produces a confusing error from a subprocess; this produces a clear one.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("vm.name must not be empty");
        }
        if self.cpus == 0 {
            bail!("vm.cpus must be at least 1");
        }
        if self.memory_mib < 1024 {
            bail!(
                "vm.memory_mib is {} — a Windows guest needs at least 1024, and realistically 4096",
                self.memory_mib
            );
        }
        if self.disk_gib == 0 {
            bail!("vm.disk_gib must be at least 1");
        }

        if let Some(iso) = &self.install_iso {
            if !iso.exists() {
                bail!("install_iso does not exist: {}", iso.display());
            }
        }
        if let Some(iso) = &self.driver_iso {
            if !iso.exists() {
                bail!("driver_iso does not exist: {}", iso.display());
            }
        }

        if self.state_dir.is_none() && self.name.contains('/') {
            bail!("vm.name must not contain a path separator when state_dir is unset");
        }

        Ok(())
    }

    /// Directory holding runtime state for this instance.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            let base = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            base.join("wvm").join(&self.name)
        })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_dir().join("qemu.pid")
    }

    /// QMP socket: the control channel to a running QEMU, used for lifecycle operations rather
    /// than signals.
    pub fn qmp_socket(&self) -> PathBuf {
        self.state_dir().join("qmp.sock")
    }

    /// Serial console log. QEMU writes boot output here; it is the first place to look when a VM
    /// fails to start, and it exists whether or not a display is attached.
    pub fn serial_log(&self) -> PathBuf {
        self.state_dir().join("serial.log")
    }

    pub fn monitor_log(&self) -> PathBuf {
        self.state_dir().join("qemu.log")
    }

    /// Build the QEMU argument vector.
    ///
    /// Kept as a pure function of the config so it can be asserted in tests and printed with
    /// `wvm vm cmdline` without launching anything. A command line you can read before running
    /// it is worth more than one assembled invisibly at spawn time.
    pub fn qemu_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-name".into(),
            self.name.clone(),
            // KVM with the host CPU model: without it, performance is unusably slow and Windows
            // may refuse to boot under emulation.
            "-machine".into(),
            "q35,accel=kvm".into(),
            "-cpu".into(),
            "host".into(),
            "-smp".into(),
            self.cpus.to_string(),
            "-m".into(),
            self.memory_mib.to_string(),
        ];

        // UEFI firmware. `pflash` needs the code image read-only and a writable vars copy;
        // without the vars copy, settings do not persist across boots.
        if self.firmware == Firmware::Uefi {
            let code = "/usr/share/OVMF/OVMF_CODE_4M.fd";
            let vars = self.state_dir().join("OVMF_VARS.fd");
            a.push("-drive".into());
            a.push(format!("if=pflash,format=raw,readonly=on,file={code}"));
            a.push("-drive".into());
            a.push(format!("if=pflash,format=raw,file={}", vars.display()));
        }

        // Main disk: virtio-blk. Much faster than emulated IDE/SATA, at the cost of requiring
        // the virtio driver ISO during installation.
        a.push("-drive".into());
        a.push(format!(
            "file={},if=none,id=disk0,format=qcow2,cache=writeback,discard=unmap",
            self.disk.display()
        ));
        a.push("-device".into());
        a.push("virtio-blk-pci,drive=disk0,bootindex=1".into());

        // Optical drives.
        //
        // The bus choice here was arrived at by probing rather than assumption, after three wrong
        // guesses. The constraint that finally settled it is not performance — it is a circular
        // dependency:
        //
        //   Windows Setup has no virtio-scsi driver at install time. Putting the driver disc on
        //   the virtio-scsi controller therefore makes it unreadable to the very Setup that needs
        //   to read it. The disc must sit on a bus Windows understands natively.
        //
        // So both discs go on the q35 AHCI bus, which is emulated SATA: slow, but universally
        // readable. The OS disk stays on virtio-blk, which Windows also cannot see until the
        // driver loads — and that is fine, because loading that driver is precisely what the
        // driver disc is for. The sequence is: Setup sees the AHCI discs, you point it at
        // viostor on the driver disc, the virtio-blk disk appears, you install to it.
        //
        // Two earlier findings still apply and are regression-tested:
        //   * One CD per IDE *unit*. Two on the same unit fails with "Can't create IDE unit 1,
        //     bus supports only 1 units" — and QEMU creates the QMP socket before exiting, so it
        //     presents as "running but unresponsive". Separate ports (ide.0, ide.1) are fine.
        //   * QEMU's argument diagnostics go to stderr while the QMP socket still appears; the
        //     supervisor reports the unresponsive state rather than inventing a cause, and
        //     `wvm vm log` points at the real message.
        if let Some(iso) = &self.install_iso {
            a.push("-drive".into());
            a.push(format!(
                "file={},media=cdrom,readonly=on,if=none,id=cd0",
                iso.display()
            ));
            a.push("-device".into());
            a.push("ide-cd,drive=cd0,bus=ide.0,bootindex=2".into());
        }

        // virtio-win drivers: the disc that makes the virtio devices visible. On its own port, on
        // the bus Setup can already read. Read-only, and never bootable — it is not an OS.
        if let Some(iso) = &self.driver_iso {
            a.push("-drive".into());
            a.push(format!(
                "file={},media=cdrom,readonly=on,if=none,id=cd1",
                iso.display()
            ));
            a.push("-device".into());
            a.push("ide-cd,drive=cd1,bus=ide.1".into());
        }

        // Networking: user-mode with an explicit host forward, deliberately NOT bridged. The
        // guest must not be reachable from the LAN, and it must not be able to reach the LAN
        // either beyond what user-mode NAT allows.
        a.push("-netdev".into());
        a.push(format!(
            "user,id=net0,hostfwd=tcp:127.0.0.1:{}-:{}",
            self.forward_port, self.guest_port
        ));
        a.push("-device".into());
        a.push("virtio-net-pci,netdev=net0".into());

        // Headless by default (D-004). No display backend at all, so nothing depends on a
        // desktop session being present.
        a.push("-display".into());
        a.push("none".into());

        // Serial console to a file: the boot log exists whether or not anyone is watching.
        a.push("-serial".into());
        a.push(format!("file:{}", self.serial_log().display()));

        // QMP over a Unix socket, so lifecycle control is a protocol rather than a signal.
        a.push("-qmp".into());
        a.push(format!(
            "unix:{},server=on,wait=off",
            self.qmp_socket().display()
        ));

        // Whether QEMU exits when the guest reboots.
        //
        // Configurable, because the right answer differs by stage and getting it wrong is
        // disruptive rather than merely suboptimal:
        //
        //   * During installation, Windows reboots several times as part of its own process. With
        //     `-no-reboot`, QEMU exits instead of rebooting the guest, and the install appears to
        //     vanish. That is exactly what happened on the first real install here.
        //   * For a long-lived VM, the opposite is wanted: a guest-initiated reboot should not
        //     silently leave an empty VM behind, so exiting makes the stop visible.
        if self.no_reboot {
            a.push("-no-reboot".to_string());
        }

        a
    }

    /// The same arguments as a single shell-safe string, for display and for the docs.
    pub fn qemu_cmdline(&self) -> String {
        let mut out = String::from("qemu-system-x86_64");
        for arg in self.qemu_args() {
            out.push(' ');
            if arg.contains(' ') || arg.contains(',') {
                out.push('\'');
                out.push_str(&arg);
                out.push('\'');
            } else {
                out.push_str(&arg);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VmConfig {
        VmConfig {
            name: "w11".into(),
            disk: PathBuf::from("/tmp/wvm-test/disk.qcow2"),
            disk_gib: 64,
            memory_mib: 4096,
            cpus: 4,
            install_iso: None,
            driver_iso: None,
            firmware: Firmware::Uefi,
            guest_port: 48273,
            forward_port: 48274,
            state_dir: Some(PathBuf::from("/tmp/wvm-test/state")),
            no_reboot: false,
        }
    }

    #[test]
    fn a_valid_config_passes() {
        config().validate().expect("should be valid");
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut c = config();
        c.name = String::new();
        assert!(c.validate().is_err());
    }

    #[test]
    fn zero_cpus_is_rejected() {
        let mut c = config();
        c.cpus = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn absurdly_small_memory_is_rejected_with_an_explanation() {
        let mut c = config();
        c.memory_mib = 256;
        let err = c.validate().expect_err("256 MiB must be refused");
        assert!(
            err.to_string().contains("Windows guest"),
            "the error should say why: {err}"
        );
    }

    #[test]
    fn a_missing_install_iso_is_rejected() {
        let mut c = config();
        c.install_iso = Some(PathBuf::from("/nonexistent/tiny11.iso"));
        let err = c.validate().expect_err("a missing ISO must be refused");
        assert!(err.to_string().contains("install_iso"));
    }

    #[test]
    fn cmdline_uses_kvm_and_the_host_cpu() {
        let args = config().qemu_args();
        let joined = args.join(" ");
        assert!(joined.contains("q35,accel=kvm"), "KVM must be requested");
        assert!(
            joined.contains("-cpu host"),
            "the host CPU model is required"
        );
    }

    #[test]
    fn cmdline_is_headless() {
        // D-004: the plane must work with no display attached.
        let args = config().qemu_args();
        let joined = args.join(" ");
        assert!(
            joined.contains("-display none"),
            "must not require a display"
        );
    }

    #[test]
    fn cmdline_forwards_only_loopback() {
        // The guest control channel must not be exposed beyond this host.
        let joined = config().qemu_args().join(" ");
        assert!(
            joined.contains("hostfwd=tcp:127.0.0.1:48274-:48273"),
            "the forward must bind loopback explicitly: {joined}"
        );
        assert!(
            !joined.contains("hostfwd=tcp::"),
            "must not bind all interfaces"
        );
    }

    #[test]
    fn cmdline_pins_the_disk_to_virtio_blk() {
        let joined = config().qemu_args().join(" ");
        assert!(
            joined.contains("virtio-blk-pci"),
            "disk should be virtio-blk"
        );
        assert!(joined.contains("bootindex=1"), "the disk should boot first");
    }

    #[test]
    fn the_driver_iso_is_never_bootable() {
        let mut c = config();
        c.driver_iso = Some(PathBuf::from("/tmp/wvm-test/virtio-win.iso"));
        std::fs::write("/tmp/wvm-test/virtio-win.iso", b"x").ok();

        let joined = c.qemu_args().join(" ");
        assert!(joined.contains("id=cd1"), "the driver ISO must be attached");
        // The driver ISO appears with no bootindex, unlike the installer CD.
        let cd1_dev_index = joined.find("drive=cd1").expect("cd1 device");
        let following = &joined[cd1_dev_index..];
        let end = following.find(" -").unwrap_or(following.len());
        assert!(
            !following[..end].contains("bootindex"),
            "the driver ISO must not be bootable: {}",
            &following[..end]
        );
    }

    #[test]
    fn discs_go_on_the_ahci_bus_not_virtio_scsi() {
        // The constraint is a circular dependency, not performance: Windows Setup has no
        // virtio-scsi driver at install time, so a driver disc on that controller is unreadable
        // to the Setup that needs it. Both discs must therefore be on AHCI, which Windows reads
        // out of the box.
        let mut c = config();
        c.install_iso = Some(PathBuf::from("/tmp/wvm-test/tiny11.iso"));
        c.driver_iso = Some(PathBuf::from("/tmp/wvm-test/virtio-win.iso"));
        std::fs::write("/tmp/wvm-test/tiny11.iso", b"x").ok();
        std::fs::write("/tmp/wvm-test/virtio-win.iso", b"x").ok();

        let joined = c.qemu_args().join(" ");
        assert!(
            !joined.contains("virtio-scsi"),
            "the discs must not be on virtio-scsi: {joined}"
        );
        assert!(joined.contains("ide-cd,drive=cd0"), "{joined}");
        assert!(joined.contains("ide-cd,drive=cd1"), "{joined}");
    }

    #[test]
    fn each_disc_gets_its_own_ide_port() {
        // Regression, found by running it: q35 gives each IDE *unit* one device, so two discs on
        // the same unit fails with
        //   "Can't create IDE unit 1, bus supports only 1 units"
        // and — because QEMU creates the QMP socket before exiting — the failure presents as
        // "running but unresponsive" rather than as the error it is.
        //
        // Separate ports (ide.0, ide.1) are fine. This is the distinction the earlier version
        // missed: the limit is per unit, not per bus.
        let mut c = config();
        c.install_iso = Some(PathBuf::from("/tmp/wvm-test/tiny11.iso"));
        c.driver_iso = Some(PathBuf::from("/tmp/wvm-test/virtio-win.iso"));
        std::fs::write("/tmp/wvm-test/tiny11.iso", b"x").ok();
        std::fs::write("/tmp/wvm-test/virtio-win.iso", b"x").ok();

        let args = c.qemu_args();
        let buses: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("ide-cd,drive="))
            .collect();

        assert_eq!(buses.len(), 2, "both discs should be attached");

        let ports: Vec<&str> = buses
            .iter()
            .map(|d| d.split("bus=").nth(1).unwrap_or(""))
            .collect();
        assert_eq!(
            ports.len(),
            2,
            "each disc needs an explicit bus, or QEMU picks the same unit for both"
        );
        assert_ne!(
            ports[0], ports[1],
            "the two discs must be on different units: {ports:?}"
        );
    }

    #[test]
    fn the_installer_stays_bootable_and_the_driver_disc_does_not() {
        let mut c = config();
        c.install_iso = Some(PathBuf::from("/tmp/wvm-test/tiny11.iso"));
        c.driver_iso = Some(PathBuf::from("/tmp/wvm-test/virtio-win.iso"));
        std::fs::write("/tmp/wvm-test/tiny11.iso", b"x").ok();
        std::fs::write("/tmp/wvm-test/virtio-win.iso", b"x").ok();

        let joined = c.qemu_args().join(" ");
        assert!(
            joined.contains("ide-cd,drive=cd0,bus=ide.0,bootindex=2"),
            "the installer must be bootable: {joined}"
        );

        // The driver disc must carry no bootindex, or the guest could boot from a disc of drivers.
        let cd1 = joined.find("ide-cd,drive=cd1").expect("cd1 device");
        let end = joined[cd1..].find(" -").unwrap_or(joined.len() - cd1);
        assert!(
            !joined[cd1..cd1 + end].contains("bootindex"),
            "the driver disc must not be bootable: {}",
            &joined[cd1..cd1 + end]
        );
    }

    #[test]
    fn a_disc_without_a_driver_disc_still_works() {
        // During installation only the first disc is present, and afterwards the reverse. Neither
        // should depend on the other's presence.
        let mut c = config();
        c.install_iso = Some(PathBuf::from("/tmp/wvm-test/tiny11.iso"));
        std::fs::write("/tmp/wvm-test/tiny11.iso", b"x").ok();

        let joined = c.qemu_args().join(" ");
        assert!(
            joined.contains("ide-cd,drive=cd0,bus=ide.0,bootindex=2"),
            "{joined}"
        );
        assert!(
            !joined.contains("drive=cd1"),
            "no driver disc configured: {joined}"
        );

        let mut c2 = config();
        c2.driver_iso = Some(PathBuf::from("/tmp/wvm-test/virtio-win.iso"));
        std::fs::write("/tmp/wvm-test/virtio-win.iso", b"x").ok();
        let joined2 = c2.qemu_args().join(" ");
        assert!(joined2.contains("ide-cd,drive=cd1,bus=ide.1"), "{joined2}");
        assert!(!joined2.contains("drive=cd0"), "{joined2}");
    }

    #[test]
    fn no_reboot_is_absent_by_default() {
        // Regression, found on the first real install: Windows reboots several times during setup.
        // With `-no-reboot` present, QEMU exits instead of rebooting the guest, so the install
        // appeared to vanish mid-flight. The default must therefore be "let it reboot".
        let c = config();
        let joined = c.qemu_args().join(" ");
        assert!(
            !joined.contains("-no-reboot"),
            "a default VM must reboot when the guest asks: {joined}"
        );
    }

    #[test]
    fn no_reboot_is_honoured_when_asked_for() {
        // The other half: a long-lived VM may want the opposite, so the flag must still work.
        let mut c = config();
        c.no_reboot = true;
        let joined = c.qemu_args().join(" ");
        assert!(
            joined.contains("-no-reboot"),
            "opting in must add the flag: {joined}"
        );
    }

    #[test]
    fn cmdline_exposes_a_qmp_socket() {
        let joined = config().qemu_args().join(" ");
        assert!(
            joined.contains("-qmp unix:"),
            "lifecycle control is over QMP"
        );
    }

    #[test]
    fn state_paths_are_inside_the_state_dir() {
        let c = config();
        let dir = c.state_dir();
        assert!(c.pid_file().starts_with(&dir));
        assert!(c.qmp_socket().starts_with(&dir));
        assert!(c.serial_log().starts_with(&dir));
    }

    #[test]
    fn cmdline_is_readable_before_running() {
        let line = config().qemu_cmdline();
        assert!(line.starts_with("qemu-system-x86_64 "));
        assert!(line.contains("accel=kvm"));
    }

    #[test]
    fn toml_round_trips() {
        let c = config();
        let text = toml::to_string(&c).unwrap();
        let back: VmConfig = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn defaults_apply_when_optional_fields_are_omitted() {
        let text = r#"
            name = "minimal"
            disk = "/tmp/wvm-test/minimal.qcow2"
        "#;
        let c: VmConfig = toml::from_str(text).unwrap();
        assert_eq!(c.memory_mib, 4096);
        assert_eq!(c.cpus, 4);
        assert_eq!(c.disk_gib, 64);
        assert_eq!(c.firmware, Firmware::Uefi);
        assert_eq!(c.guest_port, 48273);
    }
}
