//! [`VmLauncherConfig`]: everything a machine is built from. No request
//! names a path, a disk, a kernel or a device; only the workspace id, the
//! generation and one of the configured tiers come from the caller.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use agentkit_sandbox_proto::launcher::Tier;
use agentkit_sandbox_proto::{MACHINE_DIR_NAME_BYTES, valid_sha256};

use crate::error::VmConfigError;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// File name of the launcher's own socket in the socket root.
pub(crate) const LISTEN_SOCKET: &str = "launcher.sock";
/// File names of a machine's sockets, which the adapter binds in
/// `<socket_root>/<machine_dir_name(ws, gen)>/`.
pub(crate) const CONTROL_SOCKET: &str = "control.sock";
pub(crate) const EGRESS_SOCKET: &str = "egress.sock";

/// `sun_path` of a unix socket address, its terminating NUL included.
#[cfg(target_os = "linux")]
pub(crate) const SUN_PATH_BYTES: usize = 108;
#[cfg(not(target_os = "linux"))]
pub(crate) const SUN_PATH_BYTES: usize = 104;

/// Most vCPUs a tier may name.
const VCPUS_MAX: u32 = 64;
/// Smallest and largest guest memory of a tier.
const MEMORY_MIN: u64 = 256 * MIB;
const MEMORY_MAX: u64 = TIB;
/// Largest process count of a tier (Linux `pid_max` ceiling).
const PIDS_MAX: u32 = 4_194_304;
/// Smallest workspace and home disks; both are at most 16 TiB.
const WORKSPACE_DISK_MIN: u64 = GIB;
const HOME_DISK_MIN: u64 = 256 * MIB;
const DISK_MAX: u64 = 16 * TIB;
/// Longest stop grace a request may get.
const STOP_GRACE_MAX_MS: u64 = 10 * 60 * 1000;
/// Longest wait for a helper's `connected`.
const CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(600);
/// Most concurrent client connections and machines.
const CONNECTIONS_MAX: usize = 1024;
const MACHINES_MAX: usize = 64;

/// A read-only input file, pinned by SHA-256 (64 lowercase hex digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Expected SHA-256 of the whole file.
    pub sha256: String,
}

/// Configuration of a [`crate::VmLauncher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmLauncherConfig {
    /// Holds `workspaces/<ws>/{workspace,home}.img` and `state/`. Created
    /// with mode 0700 when absent; must be owned by this user, not a link,
    /// and grant nothing to group or others.
    ///
    /// Restriction: only this directory itself is checked, not its
    /// ancestors (macOS's per-user `$TMPDIR` lies below the `/var` link).
    /// Every ancestor must be writable by this user and root only, as
    /// `$HOME/Library/...` and `$TMPDIR` are: another user who can write an
    /// ancestor can swap the whole root.
    pub data_root: PathBuf,
    /// Short per-user directory (mode 0700, same rules and the same
    /// ancestor restriction as `data_root`) holding the launcher's socket
    /// `launcher.sock` and one directory per machine,
    /// `machine_dir_name(ws, gen)`, for the adapter's control and egress
    /// sockets. The adapter's `socket_dir` is this directory.
    pub socket_root: PathBuf,
    /// The signed `agentkit-sandbox-vmhelper` binary.
    pub helper: PathBuf,
    /// The arm64 kernel `Image`.
    pub kernel: ImageFile,
    /// The ext4 root disk. Its digest is the machines' image digest,
    /// `sha256:<root sha256>` ([`VmLauncherConfig::image_digest`]).
    pub root_disk: ImageFile,
    /// Size of each workspace disk, created sparse (default 20 GiB).
    pub workspace_disk_bytes: u64,
    /// Size of each home disk, created sparse (default 4 GiB).
    pub home_disk_bytes: u64,
    /// Most helpers running at once, across workspaces (default 4). An
    /// `ensure` beyond it is refused (`runtime_failed`) and records nothing.
    pub max_machines: usize,
    /// Tiers a request may name, by value. A machine gets
    /// `ceil(cpus_milli / 1000)` vCPUs and `memory_bytes` of memory.
    /// Default: one tier of 2 CPUs, 2 GiB and 512 processes.
    pub tiers: BTreeMap<String, Tier>,
    /// How long `ensure` waits for a new helper's `connected` (default
    /// 60 s, above the helper's own 30 s vsock deadline). Must be shorter
    /// than the adapter's `launcher_timeout`.
    pub connect_timeout: Duration,
    /// Longest stop grace a request gets before the helper is killed, in
    /// milliseconds (default 30 s).
    pub stop_grace_max_ms: u64,
    /// Client connections served at once (default 32).
    pub max_connections: usize,
}

impl VmLauncherConfig {
    /// A configuration with the defaults above.
    pub fn new(
        data_root: PathBuf,
        socket_root: PathBuf,
        helper: PathBuf,
        kernel: ImageFile,
        root_disk: ImageFile,
    ) -> Self {
        let tier = Tier {
            cpus_milli: 2000,
            memory_bytes: 2 * GIB,
            pids: 512,
        };
        Self {
            data_root,
            socket_root,
            helper,
            kernel,
            root_disk,
            workspace_disk_bytes: 20 * GIB,
            home_disk_bytes: 4 * GIB,
            max_machines: 4,
            tiers: BTreeMap::from([("default".to_owned(), tier)]),
            connect_timeout: Duration::from_secs(60),
            stop_grace_max_ms: 30_000,
            max_connections: 32,
        }
    }

    /// The image digest of every machine: `sha256:` and the root disk's
    /// SHA-256. `ensure` must name it, the kernel command line carries it
    /// (`agentkit.image=`) and the guest answers it in `hello`.
    pub fn image_digest(&self) -> String {
        format!("sha256:{}", self.root_disk.sha256)
    }

    /// The launcher's own socket, `<socket_root>/launcher.sock`: the
    /// adapter's `launcher_socket`.
    pub fn listen_socket(&self) -> PathBuf {
        self.socket_root.join(LISTEN_SOCKET)
    }

    /// Refuse paths that are relative, not plain, not UTF-8 or too long for
    /// a unix socket address, malformed digests, and sizes, tiers and
    /// bounds out of range.
    pub fn validate(&self) -> Result<(), VmConfigError> {
        self.validate_paths()?;
        self.validate_tiers()?;
        let checks = [
            (
                valid_sha256(&self.kernel.sha256) && valid_sha256(&self.root_disk.sha256),
                "kernel and root_disk sha256 must be 64 lowercase hex digits",
            ),
            (
                in_mib_range(self.workspace_disk_bytes, WORKSPACE_DISK_MIN),
                "workspace_disk_bytes must be a whole number of MiB between 1 GiB and 16 TiB",
            ),
            (
                in_mib_range(self.home_disk_bytes, HOME_DISK_MIN),
                "home_disk_bytes must be a whole number of MiB between 256 MiB and 16 TiB",
            ),
            (
                (1..=MACHINES_MAX).contains(&self.max_machines),
                "max_machines must be between 1 and 64",
            ),
            (
                !self.connect_timeout.is_zero() && self.connect_timeout <= CONNECT_TIMEOUT_MAX,
                "connect_timeout must be positive and at most 10 minutes",
            ),
            (
                self.stop_grace_max_ms <= STOP_GRACE_MAX_MS,
                "stop_grace_max_ms must be at most 10 minutes",
            ),
            (
                (1..=CONNECTIONS_MAX).contains(&self.max_connections),
                "max_connections must be between 1 and 1024",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, problem)) => Err(VmConfigError::Invalid(problem)),
            None => Ok(()),
        }
    }

    fn validate_paths(&self) -> Result<(), VmConfigError> {
        let paths = [
            &self.data_root,
            &self.socket_root,
            &self.helper,
            &self.kernel.path,
            &self.root_disk.path,
        ];
        if !paths.iter().all(|path| plain_absolute(path)) {
            return Err(VmConfigError::Invalid(
                "every path must be absolute, UTF-8 and without . or .. components",
            ));
        }
        if !sockets_fit(&self.socket_root, SUN_PATH_BYTES) {
            return Err(VmConfigError::Invalid(
                "socket_root is too long for its sockets to fit a unix socket address",
            ));
        }
        Ok(())
    }

    fn validate_tiers(&self) -> Result<(), VmConfigError> {
        let tier_ok = |tier: &Tier| {
            (1..=VCPUS_MAX).contains(&vcpus(tier))
                && (MEMORY_MIN..=MEMORY_MAX).contains(&tier.memory_bytes)
                && tier.memory_bytes.is_multiple_of(MIB)
                && (1..=PIDS_MAX).contains(&tier.pids)
        };
        if self.tiers.is_empty() {
            return Err(VmConfigError::Invalid("at least one tier is required"));
        }
        if self
            .tiers
            .iter()
            .any(|(name, tier)| name.is_empty() || !tier_ok(tier))
        {
            return Err(VmConfigError::Invalid(
                "a tier needs cpus_milli up to 64000, memory_bytes in whole MiB from 256 MiB to 1 TiB and pids 1..=4194304",
            ));
        }
        Ok(())
    }

    /// The machine shape of `tier` when it equals a configured tier.
    pub(crate) fn shape_of(&self, tier: &Tier) -> Option<Shape> {
        self.tiers
            .values()
            .any(|allowed| allowed == tier)
            .then(|| Shape {
                cpus: vcpus(tier),
                memory_bytes: tier.memory_bytes,
            })
    }
}

/// What a tier gives a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) cpus: u32,
    pub(crate) memory_bytes: u64,
}

/// vCPUs of a tier: its CPU quota rounded up to whole CPUs.
fn vcpus(tier: &Tier) -> u32 {
    tier.cpus_milli.div_ceil(1000)
}

fn in_mib_range(bytes: u64, min: u64) -> bool {
    (min..=DISK_MAX).contains(&bytes) && bytes.is_multiple_of(MIB)
}

/// Absolute, UTF-8, only plain components and spelled the way it
/// normalizes (no `.`, no doubled or trailing `/`).
fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some()
        && path != Path::new("/")
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// True when the launcher's socket and every machine socket below
/// `socket_root` fit `sun_path` (with its NUL) as spelled. The machine
/// directory name has a fixed length whatever the workspace and generation.
pub(crate) fn sockets_fit(socket_root: &Path, sun_path: usize) -> bool {
    let root = socket_root.as_os_str().len();
    let longest_name = CONTROL_SOCKET.len().max(EGRESS_SOCKET.len());
    let machine = root + 1 + MACHINE_DIR_NAME_BYTES + 1 + longest_name + 1;
    let listen = root + 1 + LISTEN_SOCKET.len() + 1;
    machine.max(listen) <= sun_path
}

#[cfg(test)]
mod tests;
