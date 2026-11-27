//! The six launcher requests over helper processes. Every request checks
//! its workspace id and generation before it builds a name or path;
//! `ensure`, `stop`, `reset_home` and `destroy` hold the workspace's lock
//! for their whole run. At most one machine per workspace is held: the
//! running one, or one whose helper exited and that no stop has proven
//! gone yet.

mod ensure;
mod stop;
mod storage;

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use agentkit_sandbox_proto::launcher::{Reply, Request};

use crate::config::VmLauncherConfig;
use crate::dirs::SocketDirs;
use crate::disks::Disks;
use crate::error::bounded;
use crate::images::Images;
use crate::locks::WorkspaceLocks;
use crate::machine::Machine;
use crate::naming::WorkspaceId;
use crate::state::StateStore;

/// The request handler shared by every connection.
pub(crate) struct Service {
    config: Arc<VmLauncherConfig>,
    image_digest: String,
    images: Images,
    state: StateStore,
    locks: WorkspaceLocks,
    machines: StdMutex<HashMap<String, Arc<Machine>>>,
    disks: Disks,
    sockets: SocketDirs,
    /// Held from the capacity count to the machine's insertion, so two
    /// workspaces never both take the last of `max_machines`.
    creating: tokio::sync::Mutex<()>,
    /// The socket root's lock: no second launcher binds there while this
    /// service lives.
    _socket_lock: std::fs::File,
}

impl Service {
    pub(crate) fn new(
        config: Arc<VmLauncherConfig>,
        images: Images,
        state: StateStore,
        socket_lock: std::fs::File,
        own_uid: u32,
    ) -> Self {
        let disks = Disks::new(
            config.data_root.join("workspaces"),
            own_uid,
            config.workspace_disk_bytes,
            config.home_disk_bytes,
        );
        let sockets = SocketDirs::new(config.socket_root.clone(), own_uid);
        Self {
            image_digest: config.image_digest(),
            config,
            images,
            state,
            locks: WorkspaceLocks::default(),
            machines: StdMutex::new(HashMap::new()),
            disks,
            sockets,
            creating: tokio::sync::Mutex::new(()),
            _socket_lock: socket_lock,
        }
    }

    /// Answer one request. Never panics on request content; every failure
    /// is an error reply.
    pub(crate) async fn handle(&self, request: Request) -> Reply {
        let summary = describe(&request);
        let outcome = match request {
            Request::Ensure {
                workspace_id,
                generation,
                image_digest,
                tier,
            } => self
                .ensure(&workspace_id, generation, &image_digest, &tier)
                .await
                .map(|started| Reply::Ensured {
                    container_id: started.machine_id,
                    channel_token: started.channel_token,
                }),
            Request::Stop {
                workspace_id,
                generation,
                grace_ms,
            } => self.stop(&workspace_id, generation, grace_ms).await,
            Request::Inspect { workspace_id } => self.inspect(&workspace_id).await,
            Request::Usage { workspace_id } => self.usage(&workspace_id),
            Request::ResetHome { workspace_id } => {
                self.reset_home(&workspace_id).await.map(|()| Reply::Done)
            }
            Request::Destroy { workspace_id } => {
                self.destroy(&workspace_id).await.map(|()| Reply::Done)
            }
        };
        let reply = outcome.unwrap_or_else(|error| Reply::Error(error.to_wire()));
        if let Some(summary) = summary {
            log(&format!("{summary}: {}", outcome_text(&reply)));
        }
        reply
    }

    /// The machine held for `ws`, running or exited.
    fn machine(&self, ws: &WorkspaceId) -> Option<Arc<Machine>> {
        self.machines_lock().get(ws.as_str()).cloned()
    }

    fn insert(&self, ws: &WorkspaceId, machine: Arc<Machine>) {
        self.machines_lock().insert(ws.as_str().to_owned(), machine);
    }

    /// Forget `machine` of `ws` (and nothing that replaced it).
    fn remove(&self, ws: &WorkspaceId, machine: &Arc<Machine>) {
        let mut machines = self.machines_lock();
        if machines
            .get(ws.as_str())
            .is_some_and(|held| Arc::ptr_eq(held, machine))
        {
            machines.remove(ws.as_str());
        }
    }

    /// Helpers whose process has not exited.
    fn running_count(&self) -> usize {
        self.machines_lock()
            .values()
            .filter(|m| m.running())
            .count()
    }

    /// Every workspace with a held machine, and its generation.
    fn held(&self) -> Vec<(String, u64)> {
        let machines = self.machines_lock();
        machines
            .iter()
            .map(|(ws, machine)| (ws.clone(), machine.generation))
            .collect()
    }

    fn machines_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Machine>>> {
        self.machines.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The id `ensured` answers for a machine. The adapter only shows it.
fn machine_id(machine: &Machine) -> String {
    format!("vm-g{}-p{}", machine.generation, machine.pid)
}

/// What a mutating request names, for the log; `None` for reads.
fn describe(request: &Request) -> Option<String> {
    let text = match request {
        Request::Ensure {
            workspace_id,
            generation,
            ..
        } => format!("ensure {} g{generation}", bounded(workspace_id)),
        Request::Stop {
            workspace_id,
            generation,
            ..
        } => format!("stop {} g{generation}", bounded(workspace_id)),
        Request::ResetHome { workspace_id } => format!("reset_home {}", bounded(workspace_id)),
        Request::Destroy { workspace_id } => format!("destroy {}", bounded(workspace_id)),
        Request::Inspect { .. } | Request::Usage { .. } => return None,
    };
    Some(text)
}

fn outcome_text(reply: &Reply) -> String {
    match reply {
        // Never the channel token.
        Reply::Ensured { container_id, .. } => format!("ensured {container_id}"),
        Reply::Stopped { proven, reason } => match reason {
            Some(reason) => format!("stopped proven={proven} ({reason})"),
            None => format!("stopped proven={proven}"),
        },
        Reply::Error(error) => format!("refused {:?}: {}", error.code, error.message),
        other => format!("{other:?}"),
    }
}

/// One line on stderr. Names workspaces, generations and pids; never a
/// token or a helper's configuration.
pub(crate) fn log(message: &str) {
    eprintln!("agentkit-sandbox-microvm: {message}");
}
