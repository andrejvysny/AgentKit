//! What the adapter needs from its deployment: where the launcher listens,
//! where the per-machine socket directories are, the bounds of every wait,
//! and the egress limits of each machine.

use std::path::PathBuf;
use std::time::Duration;

use agentkit_egress::{EgressConfig, EgressError};
use agentkit_sandbox_proto::MACHINE_DIR_NAME_BYTES;

use crate::start::{CONTROL_SOCKET, EGRESS_SOCKET};

/// `sun_path` of a unix socket address, its terminating NUL included.
/// A platform constant; it belongs in `agentkit-sys` with the other
/// platform facts, which is not being changed now, so it stays here.
#[cfg(target_os = "linux")]
const SUN_PATH_BYTES: usize = 108;
#[cfg(not(target_os = "linux"))]
const SUN_PATH_BYTES: usize = 104;

/// Longest `socket_dir`, in bytes, whose machine sockets
/// `<socket_dir>/<machine_dir_name>/<name>` fit `sun_path` (with its NUL)
/// for the longer socket name. The machine directory name has a fixed
/// length whatever the workspace id and generation, so this holds for
/// every machine.
const fn socket_dir_max_bytes(sun_path: usize) -> usize {
    let name = if CONTROL_SOCKET.len() > EGRESS_SOCKET.len() {
        CONTROL_SOCKET.len()
    } else {
        EGRESS_SOCKET.len()
    };
    // "/" machine "/" name, then the NUL.
    let tail = 1 + MACHINE_DIR_NAME_BYTES + 1 + name + 1;
    sun_path.saturating_sub(tail)
}

/// Configuration of a [`crate::ContainerBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerBackendConfig {
    /// The launcher's unix socket.
    pub launcher_socket: PathBuf,
    /// The adapter's view of the launcher's socket directory: the machine
    /// of workspace `ws` at generation `g` binds its control and egress
    /// sockets in `<socket_dir>/<machine_dir_name(ws, g)>/`, which the
    /// launcher creates. The server's user must be able to search
    /// `socket_dir` (the launcher's `server_gid`).
    pub socket_dir: PathBuf,
    /// Time from the launcher's `ensured` to a greeted control channel: the
    /// one control connection must arrive and answer `hello` within it.
    pub handshake_timeout: Duration,
    /// Time one guest request may take, from queueing to its whole reply.
    /// Also bounds waiting for `bye` at a stop.
    pub request_timeout: Duration,
    /// Time one launcher request may take; must exceed the stop grace.
    pub launcher_timeout: Duration,
    /// Grace the launcher gives a container before it kills it at a stop.
    pub stop_grace_ms: u64,
    /// The image digest the deployment installed. When set, a machine spec
    /// naming another digest is refused without asking the launcher.
    pub image_digest: Option<String>,
    /// Limits of each machine's egress proxy (NET-8).
    pub egress: EgressConfig,
    /// `Some` when the launcher starts microVMs (ADR 0040): the workspace
    /// and home disk sizes added up. `describe` then answers kind
    /// `microvm` with a hard disk quota, and storage usage reports this
    /// quota. Usage counts both trees (the guest's walk of `/workspace`
    /// and `/home/agent`, or the launcher's allocated bytes of both
    /// disks), so the quota covers both disks too. `None` (the default) is
    /// the container launcher.
    pub microvm_disk_bytes: Option<u64>,
}

/// Why a configuration cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ContainerConfigError {
    /// A bound or path is unusable.
    #[error("invalid container backend config: {0}")]
    Invalid(&'static str),
    /// The egress limits are unusable.
    #[error(transparent)]
    Egress(#[from] EgressError),
}

impl ContainerBackendConfig {
    /// The ADR's defaults: a 15 s handshake (the supervisor retries its
    /// dial for 30 s), 30 s per guest request, 120 s per launcher request,
    /// a 10 s stop grace, no pinned digest and the default egress limits.
    pub fn new(launcher_socket: PathBuf, socket_dir: PathBuf) -> Self {
        Self {
            launcher_socket,
            socket_dir,
            handshake_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(30),
            launcher_timeout: Duration::from_secs(120),
            stop_grace_ms: 10_000,
            image_digest: None,
            egress: EgressConfig::default(),
            microvm_disk_bytes: None,
        }
    }

    /// Refuse relative paths (they would depend on the working directory),
    /// a `socket_dir` too long for its machine sockets to fit a unix socket
    /// address (65 bytes on macOS, 69 on Linux), zero waits, a launcher
    /// wait no longer than the stop grace, and unusable egress limits.
    pub fn validate(&self) -> Result<(), ContainerConfigError> {
        let grace = Duration::from_millis(self.stop_grace_ms);
        let checks = [
            (
                self.launcher_socket.is_absolute(),
                "launcher_socket must be an absolute path",
            ),
            (
                self.socket_dir.is_absolute(),
                "socket_dir must be an absolute path",
            ),
            (
                self.socket_dir.as_os_str().len() <= socket_dir_max_bytes(SUN_PATH_BYTES),
                "socket_dir is too long for its machine sockets to fit a unix socket path",
            ),
            (
                !self.handshake_timeout.is_zero(),
                "handshake_timeout must be positive",
            ),
            (
                !self.request_timeout.is_zero(),
                "request_timeout must be positive",
            ),
            (
                self.launcher_timeout > grace,
                "launcher_timeout must be longer than stop_grace_ms",
            ),
            (
                self.image_digest.as_ref().is_none_or(|d| !d.is_empty()),
                "image_digest must not be empty",
            ),
            (
                self.microvm_disk_bytes != Some(0),
                "microvm_disk_bytes must be positive",
            ),
        ];
        if let Some((_, problem)) = checks.iter().find(|(ok, _)| !ok) {
            return Err(ContainerConfigError::Invalid(problem));
        }
        self.egress.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContainerBackendConfig {
        ContainerBackendConfig::new("/run/launcher.sock".into(), "/run/sockets".into())
    }

    #[test]
    fn defaults_are_valid() {
        assert!(config().validate().is_ok());
    }

    #[test]
    fn unusable_bounds_are_refused() {
        let relative = ContainerBackendConfig {
            socket_dir: "sockets".into(),
            ..config()
        };
        let short = ContainerBackendConfig {
            launcher_timeout: Duration::from_secs(5),
            ..config()
        };
        let zero = ContainerBackendConfig {
            request_timeout: Duration::ZERO,
            ..config()
        };
        let no_disk = ContainerBackendConfig {
            microvm_disk_bytes: Some(0),
            ..config()
        };
        for bad in [relative, short, zero, no_disk] {
            assert!(matches!(
                bad.validate(),
                Err(ContainerConfigError::Invalid(_))
            ));
        }
        let long = ContainerBackendConfig {
            socket_dir: format!("/{}", "d".repeat(SUN_PATH_BYTES)).into(),
            ..config()
        };
        assert!(matches!(
            long.validate(),
            Err(ContainerConfigError::Invalid(why)) if why.contains("socket_dir is too long")
        ));
        let mut egress = config();
        egress.egress.max_tunnels = 0;
        assert!(matches!(
            egress.validate(),
            Err(ContainerConfigError::Egress(_))
        ));
    }

    #[test]
    fn socket_dir_bound_leaves_room_for_both_socket_paths() {
        assert_eq!(socket_dir_max_bytes(104), 65);
        assert_eq!(socket_dir_max_bytes(108), 69);
        let max = socket_dir_max_bytes(SUN_PATH_BYTES);
        let dir = format!("/{}", "d".repeat(max - 1));
        let machine = agentkit_sandbox_proto::machine_dir_name(&"w".repeat(64), u64::MAX);
        let path = format!("{dir}/{machine}/{CONTROL_SOCKET}");
        assert_eq!(path.len() + 1, SUN_PATH_BYTES, "the longest path just fits");
        let fits = ContainerBackendConfig {
            socket_dir: dir.clone().into(),
            ..config()
        };
        assert!(fits.validate().is_ok());
        let over = ContainerBackendConfig {
            socket_dir: format!("{dir}x").into(),
            ..config()
        };
        assert!(over.validate().is_err());
    }

    /// The WP-2.1 deployments: Docker's and rootless Podman's socket
    /// directories.
    #[test]
    fn the_deployed_socket_directories_fit() {
        for dir in ["/srv/agentkit/sockets", "/run/user/1001/agentkit/sockets"] {
            let config = ContainerBackendConfig::new("/run/launcher.sock".into(), dir.into());
            assert!(config.validate().is_ok(), "{dir}");
        }
    }
}
