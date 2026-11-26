//! Starting a machine (ADR 0039, "Backend adapter"): the launcher starts
//! the container, creates `<socket_dir>/<machine_dir_name(ws, gen)>/` and
//! answers the container's channel token; the adapter then binds the
//! control and egress sockets there (mode 0666: the container's uid must
//! connect, and nobody else reaches them, since the launcher's socket
//! directory grants others nothing) and greets the guest.
//!
//! The handshake: the adapter accepts control connections until one
//! answers `hello` with our protocol, the generation, the spec's image
//! digest and the channel token (compared in constant time). Candidates
//! are greeted concurrently, so a peer that connects first and stays
//! silent cannot hold the real guest out, and one that answers wrong is
//! dropped. At most [`HANDSHAKE_ATTEMPTS`] connections are taken, all
//! within the handshake time. Then the listener is dropped, so a later
//! connect to the socket file (which stays until the stop, and which the
//! workload cannot replace: the mount is read-only) is refused. The
//! supervisor dials before any workload runs and retries for 30 s, so
//! binding after `ensured` is in time.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentkit_egress::EgressProxy;
use agentkit_sandbox_proto::guest::{Reply, Request};
use agentkit_sandbox_proto::{PROTOCOL_VERSION, machine_dir_name};
use agentkit_workspaces::{MachineHandle, MachineSpec, SandboxError};
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::backend::Inner;
use crate::channel::Channel;
use crate::expect::Expect;
use crate::launcher::Ensured;
use crate::machine::Machine;
use crate::mapping;
use crate::token::ChannelToken;

/// The control socket's file name in the machine directory.
pub(crate) const CONTROL_SOCKET: &str = "control.sock";
/// The egress socket's file name in the machine directory.
pub(crate) const EGRESS_SOCKET: &str = "egress.sock";
/// Mode of both socket files: the guest's uid must connect.
const SOCKET_MODE: u32 = 0o666;
/// Most control connections taken for one handshake.
pub(crate) const HANDSHAKE_ATTEMPTS: usize = 16;

/// `<socket_dir>/<machine_dir_name(ws, gen)>`, the launcher's layout.
pub(crate) fn machine_dir(socket_dir: &Path, ws: &str, generation: u64) -> PathBuf {
    socket_dir.join(machine_dir_name(ws, generation))
}

/// What the guest must answer in `hello`.
#[derive(Debug, Clone)]
struct Expected {
    generation: u64,
    image_digest: String,
    token: ChannelToken,
}

fn unavailable(spec: &MachineSpec, why: impl std::fmt::Display) -> SandboxError {
    SandboxError::Unavailable {
        reason: format!(
            "machine of {} generation {} did not start: {why}",
            spec.workspace_id, spec.generation
        ),
    }
}

impl Inner {
    /// Ask the launcher for the container, then connect to its guest. A
    /// failure after the launcher started it stops it (best effort).
    pub(crate) async fn start(&self, spec: &MachineSpec) -> Result<Arc<Machine>, SandboxError> {
        let ws = spec.workspace_id.as_str();
        let tier = mapping::tier(&spec.limits);
        let ensured = self
            .launcher
            .ensure(ws, spec.generation.0, &spec.image_digest, tier)
            .await
            .map_err(|failure| mapping::launcher(failure, Some(spec.generation)))?;
        self.registry
            .served(&spec.workspace_id, spec.generation, spec.limits);
        let dir = machine_dir(&self.config.socket_dir, ws, spec.generation.0);
        match self.connect(spec, &dir, ensured).await {
            Ok(machine) => Ok(Arc::new(machine)),
            Err(error) => {
                let _ = fs::remove_file(dir.join(CONTROL_SOCKET));
                self.stop_container(&spec.workspace_id, spec.generation)
                    .await;
                Err(error)
            }
        }
    }

    async fn connect(
        &self,
        spec: &MachineSpec,
        dir: &Path,
        ensured: Ensured,
    ) -> Result<Machine, SandboxError> {
        let deadline = Instant::now() + self.config.handshake_timeout;
        check_dir(dir).map_err(|error| unavailable(spec, error))?;
        let control_path = dir.join(CONTROL_SOCKET);
        let egress_path = dir.join(EGRESS_SOCKET);
        clear(&control_path).map_err(|error| unavailable(spec, error))?;
        clear(&egress_path).map_err(|error| unavailable(spec, error))?;
        let listener = bind_control(&control_path).map_err(|error| unavailable(spec, error))?;
        let egress =
            EgressProxy::bind(&egress_path, self.config.egress.clone(), self.ports.clone())
                .map_err(|error| unavailable(spec, error))?;
        egress.set_policy(mapping::targets(&spec.network)?);
        let expected = Expected {
            generation: spec.generation.0,
            image_digest: spec.image_digest.clone(),
            token: ensured.token,
        };
        let channel = handshake(listener, expected, deadline)
            .await
            .map_err(|why| unavailable(spec, why))?;
        let serial = self.next_serial();
        let handle = MachineHandle {
            workspace_id: spec.workspace_id.clone(),
            generation: spec.generation,
            opaque: format!("container:{}:{serial}", ensured.container_id),
        };
        Ok(Machine::new(
            handle,
            channel,
            egress,
            self.config.request_timeout,
            control_path,
            self.now.clone(),
        ))
    }
}

/// The launcher created the directory; refuse anything but a directory
/// (a link is not followed).
fn check_dir(dir: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    if meta.is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )))
    }
}

/// Remove a socket left by an earlier start of this generation.
fn clear(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn bind_control(path: &Path) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    if let Err(error) = fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE)) {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

/// Greet control connections until one is the guest; answers its channel
/// and drops the listener (and every other candidate).
async fn handshake(
    listener: UnixListener,
    expected: Expected,
    deadline: Instant,
) -> Result<Channel, String> {
    let mut candidates = JoinSet::new();
    let mut taken = 0;
    let mut last_refusal: Option<String> = None;
    loop {
        tokio::select! {
            accepted = listener.accept(), if taken < HANDSHAKE_ATTEMPTS => {
                taken += 1;
                match accepted {
                    Ok((stream, _)) => {
                        let channel = Channel::start(stream);
                        candidates.spawn(greet(channel, expected.clone(), deadline));
                    }
                    Err(error) => {
                        last_refusal = Some(format!("accepting a control connection failed: {error}"));
                    }
                }
            }
            Some(outcome) = candidates.join_next() => match outcome {
                Ok(Ok(channel)) => return Ok(channel),
                Ok(Err(why)) => last_refusal = Some(why),
                Err(error) => last_refusal = Some(format!("a handshake task failed: {error}")),
            },
            () = tokio::time::sleep_until(deadline) => {
                return Err(match last_refusal {
                    Some(why) => format!("no control connection passed the handshake in time ({why})"),
                    None => "the guest did not connect in time".to_owned(),
                });
            }
        }
        if taken >= HANDSHAKE_ATTEMPTS && candidates.is_empty() {
            let why = last_refusal.unwrap_or_default();
            return Err(format!(
                "{HANDSHAKE_ATTEMPTS} control connections failed the handshake ({why})"
            ));
        }
    }
}

/// `hello` with our protocol and generation; the peer must answer the
/// same, the spec's image digest and the container's channel token. The
/// refusal never carries the token.
async fn greet(channel: Channel, expected: Expected, deadline: Instant) -> Result<Channel, String> {
    let hello = Request::Hello {
        protocol: PROTOCOL_VERSION,
        generation: expected.generation,
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let response = channel
        .request(&hello, None, Expect::Hello, left)
        .await
        .map_err(|error| format!("no hello: {error:?}"))?;
    match response.reply {
        Reply::Hello {
            protocol,
            generation,
            image_digest,
            channel_token,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(format!("the peer speaks protocol {protocol}"));
            }
            if generation != expected.generation {
                return Err(format!("the peer serves generation {generation}"));
            }
            if image_digest != expected.image_digest {
                return Err("the peer runs another image".to_owned());
            }
            if !expected.token.matches(&channel_token) {
                return Err("the peer answered another channel token".to_owned());
            }
            Ok(channel)
        }
        Reply::Error(error) => Err(format!(
            "the peer refused hello: {}",
            mapping::code_name(error.code)
        )),
        _ => Err("the peer answered hello with another message".to_owned()),
    }
}
