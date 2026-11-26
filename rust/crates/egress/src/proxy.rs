//! The listener of one machine and the handle its owner keeps.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::config::EgressConfig;
use crate::connection;
use crate::denials::{DenialBatch, DenialLog, DenialReason, DenialRecord};
use crate::error::EgressError;
use crate::ports::EgressPorts;
use crate::stats::{Counters, EgressStats, bump};
use crate::target::Target;

/// Mode of the socket file: the guest's user must be able to connect.
/// Who can reach the file is decided by its directory, which the caller
/// owns (ADR 0039: created by the launcher, mode 0711).
const SOCKET_MODE: u32 = 0o666;
/// Pause after a failed `accept` (for example, out of descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// The granted targets, shared with every connection through a watch
/// channel so a change reaches open tunnels at once (NET-6).
pub(crate) type Policy = Arc<HashSet<Target>>;

/// State shared by the handle, the accept loop and every connection.
pub(crate) struct Shared {
    pub(crate) config: EgressConfig,
    pub(crate) ports: EgressPorts,
    pub(crate) policy: watch::Sender<Policy>,
    pub(crate) tunnels: Arc<Semaphore>,
    pub(crate) counters: Counters,
    pub(crate) cancel: CancellationToken,
    operation: Mutex<Option<String>>,
    denials: Mutex<DenialLog>,
}

impl Shared {
    /// Record a refusal with the operation marked current now (NET-5).
    pub(crate) fn deny(&self, target: &Target, reason: DenialReason) {
        let operation_id = lock(&self.operation).clone();
        let record = DenialRecord {
            seq: 0,
            host: target.host().to_owned(),
            port: target.port(),
            at_ms: self.ports.runtime.now_ms(),
            reason,
            operation_id,
        };
        lock(&self.denials).push(record);
    }
}

/// A poisoned lock only means another thread panicked mid-update of a
/// plain value; the value is still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A running egress proxy for one machine. The socket a connection arrives
/// on is the machine's identity; there is no TCP listener.
///
/// Starts with no grants: nothing leaves until [`Self::set_policy`]
/// (NET-1). Dropping it stops it like [`Self::shutdown`] without waiting.
pub struct EgressProxy {
    shared: Arc<Shared>,
    path: PathBuf,
    socket_id: (u64, u64),
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for EgressProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressProxy")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl EgressProxy {
    /// Bind a unix socket at `path` and start serving. Fails if anything
    /// exists at `path`. The socket gets mode `0o666`; its directory's
    /// ownership and mode are the caller's job and must keep other users
    /// out, since the mode is set by path after binding.
    ///
    /// Must be called inside a Tokio runtime.
    pub fn bind(
        path: &Path,
        config: EgressConfig,
        ports: EgressPorts,
    ) -> Result<Self, EgressError> {
        config.validate()?;
        let listener = UnixListener::bind(path).map_err(EgressError::Bind)?;
        let socket_id = match fs::symlink_metadata(path) {
            Ok(meta) => (meta.dev(), meta.ino()),
            Err(err) => return Err(EgressError::Bind(err)),
        };
        if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)) {
            let _ = fs::remove_file(path);
            return Err(EgressError::Permissions(err));
        }
        let shared = Arc::new(Shared {
            tunnels: Arc::new(Semaphore::new(config.max_tunnels)),
            denials: Mutex::new(DenialLog::new(config.denial_buffer)),
            policy: watch::Sender::new(Policy::default()),
            operation: Mutex::new(None),
            counters: Counters::default(),
            cancel: CancellationToken::new(),
            config,
            ports,
        });
        let accept = tokio::spawn(accept_loop(listener, shared.clone()));
        Ok(Self {
            shared,
            path: path.to_owned(),
            socket_id,
            accept: Some(accept),
        })
    }

    /// Replace the granted targets. New connections see the change at once;
    /// open tunnels whose target is no longer granted are closed promptly
    /// (NET-6). Duplicates are ignored.
    pub fn set_policy(&self, grants: Vec<Target>) {
        let policy: Policy = Arc::new(grants.into_iter().collect());
        self.shared.policy.send_replace(policy);
    }

    /// The operation stamped on denials from now on (NET-5); `None` when no
    /// operation runs.
    pub fn set_current_operation(&self, operation_id: Option<String>) {
        *lock(&self.shared.operation) = operation_id;
    }

    /// Denials recorded after `cursor`; see [`DenialBatch`].
    pub fn drain(&self, cursor: u64) -> DenialBatch {
        lock(&self.shared.denials).after(cursor)
    }

    /// Counters since start.
    pub fn stats(&self) -> EgressStats {
        let free = self.shared.tunnels.available_permits();
        let open = self.shared.config.max_tunnels.saturating_sub(free) as u64;
        let denials = lock(&self.shared.denials).total();
        self.shared.counters.snapshot(open, denials)
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting, close every connection and tunnel, wait for them to
    /// end, and remove the socket file.
    pub async fn shutdown(mut self) {
        self.shared.cancel.cancel();
        if let Some(accept) = self.accept.take() {
            let _ = accept.await;
        }
        self.remove_socket();
    }

    /// Remove the socket file only if it is still the one this proxy bound.
    fn remove_socket(&self) {
        let ours = fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.file_type().is_socket() && (meta.dev(), meta.ino()) == self.socket_id
        });
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
        self.remove_socket();
    }
}

/// Accept until cancelled, then wait for every connection to end. At most
/// `max_pending` connections read their head at once; the loop does not
/// accept another until one of them is done with its head.
async fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    let cancel = shared.cancel.clone();
    let pending = Arc::new(Semaphore::new(shared.config.max_pending));
    let mut tasks = JoinSet::new();
    loop {
        while tasks.try_join_next().is_some() {}
        let permit = tokio::select! {
            permit = pending.clone().acquire_owned() => permit,
            () = cancel.cancelled() => break,
        };
        let Ok(permit) = permit else { break };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = cancel.cancelled() => break,
        };
        match accepted {
            Ok((stream, _)) => {
                bump(&shared.counters.connections);
                tasks.spawn(connection::serve(stream, shared.clone(), permit));
            }
            Err(_) => {
                bump(&shared.counters.accept_errors);
                drop(permit);
                tokio::select! {
                    () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                    () = cancel.cancelled() => break,
                }
            }
        }
    }
    drop(listener);
    while tasks.join_next().await.is_some() {}
}

/// The head-phase slot of one connection.
pub(crate) type PendingPermit = OwnedSemaphorePermit;
