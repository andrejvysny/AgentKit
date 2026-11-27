//! What the launcher tells the supervisor through its environment, and
//! the fixed guest layout (ADR 0039, "Supervisor"; vsock links ADR 0040
//! §4).

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;

use crate::error::ConfigError;

const GENERATION: &str = "AGENTKIT_GENERATION";
const IMAGE_DIGEST: &str = "AGENTKIT_IMAGE_DIGEST";
const CONTROL_SOCKET: &str = "AGENTKIT_CONTROL_SOCKET";
const EGRESS_SOCKET: &str = "AGENTKIT_EGRESS_SOCKET";
const CHANNEL_TOKEN: &str = "AGENTKIT_CHANNEL_TOKEN";
const CONTROL_VSOCK_PORT: &str = "AGENTKIT_CONTROL_VSOCK_PORT";
const EGRESS_VSOCK_PORT: &str = "AGENTKIT_EGRESS_VSOCK_PORT";
const IMAGE_DIGEST_MAX_BYTES: usize = 256;
const CHANNEL_TOKEN_MAX_BYTES: usize = 256;
/// `VMADDR_PORT_ANY`: a wildcard that binds an ephemeral port, so not a
/// port the host could name.
const VSOCK_PORT_ANY: u32 = u32::MAX;

/// How the supervisor relates to the other processes of its own process:
/// what it may reap and what it kills when an operation ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMode {
    /// PID 1 of the container (production). When an operation ends it
    /// repeats `kill(-1, SIGKILL)` and reaps until no child is left, so
    /// every process but the supervisor ends; it reaps every child.
    Init,
    /// Not PID 1, but the only supervisor in its process (a container with
    /// an init in front). When an operation ends it kills the operation's
    /// process group and leader, then every descendant (Linux `/proc`),
    /// and it reaps every child. Orphans stay descendants only when the
    /// process is a child subreaper: call [`crate::harden_process`] first.
    Supervisor,
    /// Shares its process with other code (tests). Reaps only the children
    /// it started, kills only the operation's process group and leader,
    /// and never calls the global `sync`. A descendant that left the
    /// operation's group (`setsid`, `setpgid`) survives the operation.
    Embedded,
}

impl ProcessMode {
    /// [`ProcessMode::Init`] as PID 1, else [`ProcessMode::Supervisor`].
    #[must_use]
    pub fn detect() -> Self {
        if agentkit_sys::is_init() {
            ProcessMode::Init
        } else {
            ProcessMode::Supervisor
        }
    }
}

/// Everything the supervisor needs to serve one machine generation.
/// `Debug` leaves the channel token out.
#[derive(Clone)]
pub struct Config {
    /// The generation this guest serves; `hello` must name it.
    pub generation: u64,
    /// Digest of the image the guest runs, reported in `hello`.
    pub image_digest: String,
    /// The per-generation secret the launcher placed in the environment,
    /// answered in `hello` so the host can tell its guest from a process
    /// that reached the control socket first. Never logged.
    pub channel_token: String,
    /// Unix socket the host listens on for the control channel.
    pub control_socket: PathBuf,
    /// Unix socket of the host's egress proxy.
    pub egress_socket: PathBuf,
    /// `/workspace`.
    pub workspace_root: PathBuf,
    /// `/home/agent`.
    pub home_root: PathBuf,
    /// `KEY=VALUE` lines every command's environment is built from.
    pub env_file: PathBuf,
    /// The shell a command runs through as `<shell> -c <command>`.
    pub shell: PathBuf,
    /// Where the egress shim listens (`127.0.0.1:3128`).
    pub shim_addr: SocketAddr,
    /// Process relationship; see [`ProcessMode`].
    pub mode: ProcessMode,
}

impl Config {
    /// Read the launcher's variables from the process environment, with
    /// the fixed guest layout and [`ProcessMode::detect`].
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Config::from_env`] over any variable source.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let generation = lookup(GENERATION)
            .ok_or(ConfigError::Missing(GENERATION))?
            .parse()
            .map_err(|_| ConfigError::Invalid {
                name: GENERATION,
                reason: "not an unsigned 64-bit number",
            })?;
        let image_digest = printable(&lookup, IMAGE_DIGEST, IMAGE_DIGEST_MAX_BYTES)?;
        let channel_token = printable(&lookup, CHANNEL_TOKEN, CHANNEL_TOKEN_MAX_BYTES)?;
        let socket = |name: &'static str, default: &str| -> Result<PathBuf, ConfigError> {
            let path = PathBuf::from(lookup(name).unwrap_or_else(|| default.to_owned()));
            if !path.is_absolute() {
                return Err(ConfigError::Invalid {
                    name,
                    reason: "not an absolute path",
                });
            }
            Ok(path)
        };
        Ok(Self {
            generation,
            image_digest,
            channel_token,
            control_socket: socket(CONTROL_SOCKET, "/run/agentkit/control.sock")?,
            egress_socket: socket(EGRESS_SOCKET, "/run/agentkit/egress.sock")?,
            workspace_root: PathBuf::from("/workspace"),
            home_root: PathBuf::from("/home/agent"),
            env_file: PathBuf::from("/etc/sandbox/environment"),
            shell: PathBuf::from("/bin/bash"),
            shim_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 3128)),
            mode: ProcessMode::detect(),
        })
    }
}

/// The variable `name`: 1 to `max` printable ASCII characters. The error
/// never carries the value, which may be a secret.
fn printable(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    max: usize,
) -> Result<String, ConfigError> {
    let value = lookup(name).ok_or(ConfigError::Missing(name))?;
    if value.is_empty() || value.len() > max || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ConfigError::Invalid {
            name,
            reason: "not 1 to 256 printable ASCII characters",
        });
    }
    Ok(value)
}

/// How the supervisor reaches its host. The default is the container's
/// (ADR 0039): dial [`Config::control_socket`] and pipe egress to
/// [`Config::egress_socket`]. In a microVM guest (ADR 0040 §4, Linux only)
/// `AGENTKIT_CONTROL_VSOCK_PORT` makes it listen on that vsock port for
/// its one control connection instead, and `AGENTKIT_EGRESS_VSOCK_PORT`
/// sends egress to the host (CID 2) on that port; the socket path of a
/// link on vsock is then unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transport {
    pub(crate) control: Link,
    pub(crate) egress: Link,
}

/// One link to the host. Off Linux only [`Link::Socket`] exists, so a
/// vsock link cannot reach a platform without vsock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Link {
    /// The unix socket [`Config`] names.
    #[default]
    Socket,
    /// A vsock port: listened on for control, the host's for egress.
    #[cfg(target_os = "linux")]
    Vsock(u32),
}

impl Transport {
    /// Read the vsock variables from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Transport::from_env`] over any variable source. A vsock port set
    /// together with the socket variable of its link is a conflict, and
    /// off Linux any vsock port is refused.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let control = vsock_port(&lookup, CONTROL_VSOCK_PORT, CONTROL_SOCKET)?;
        let egress = vsock_port(&lookup, EGRESS_VSOCK_PORT, EGRESS_SOCKET)?;
        Ok(Self {
            control: link(control, CONTROL_VSOCK_PORT)?,
            egress: link(egress, EGRESS_VSOCK_PORT)?,
        })
    }
}

/// The vsock port in `name`, `None` when unset. `socket` is the variable
/// of the same link's unix socket; both set is ambiguous.
fn vsock_port(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    socket: &'static str,
) -> Result<Option<u32>, ConfigError> {
    let Some(value) = lookup(name) else {
        return Ok(None);
    };
    if lookup(socket).is_some() {
        return Err(ConfigError::Conflict { name, with: socket });
    }
    match value.parse::<u32>() {
        Ok(port) if port != VSOCK_PORT_ANY => Ok(Some(port)),
        _ => Err(ConfigError::Invalid {
            name,
            reason: "not a vsock port (0 to 4294967294)",
        }),
    }
}

#[cfg(target_os = "linux")]
fn link(port: Option<u32>, _name: &'static str) -> Result<Link, ConfigError> {
    Ok(port.map_or(Link::Socket, Link::Vsock))
}

#[cfg(not(target_os = "linux"))]
fn link(port: Option<u32>, name: &'static str) -> Result<Link, ConfigError> {
    match port {
        None => Ok(Link::Socket),
        Some(_) => Err(ConfigError::Unsupported(name)),
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("generation", &self.generation)
            .field("image_digest", &self.image_digest)
            .field("channel_token", &"<redacted>")
            .field("control_socket", &self.control_socket)
            .field("egress_socket", &self.egress_socket)
            .field("workspace_root", &self.workspace_root)
            .field("home_root", &self.home_root)
            .field("env_file", &self.env_file)
            .field("shell", &self.shell)
            .field("shim_addr", &self.shim_addr)
            .field("mode", &self.mode)
            .finish()
    }
}

#[cfg(test)]
mod tests;
