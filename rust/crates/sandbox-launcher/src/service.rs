//! The launcher's six requests over a [`RuntimeApi`]. Every request checks
//! its workspace id and generation before it builds a name or path;
//! `ensure`, `stop`, `reset_home` and `destroy` hold the workspace's lock
//! for their whole run.
//!
//! What the launcher reads from the runtime it re-checks itself: a listed
//! container belongs to a workspace only when its workspace label says so,
//! and a container whose generation label is missing or malformed blocks
//! every decision that needs the workspace empty or a generation absent.

mod ensure;
mod stop;
mod storage;

use std::sync::Arc;

use agentkit_sandbox_proto::launcher::{Reply, Request};

use crate::config::Config;
use crate::dirs::SocketDirs;
use crate::error::{ServiceError, bounded};
use crate::locks::WorkspaceLocks;
use crate::naming::{
    LABEL_GENERATION, LABEL_IMAGE, LABEL_WORKSPACE, Names, WorkspaceId, managed_filter,
    parse_generation_label, workspace_filter,
};
use crate::runtime::{ContainerSummary, RuntimeApi, RuntimeError};
use crate::state::StateStore;
use crate::trust::TrustRoot;
use crate::usage::UsageCache;

/// The request handler shared by every connection.
pub(crate) struct Service {
    config: Arc<Config>,
    runtime: Arc<dyn RuntimeApi>,
    state: StateStore,
    locks: WorkspaceLocks,
    usage: UsageCache,
    dirs: SocketDirs,
    names: Names,
    /// Where checked directory walks start (the state directory's).
    trust: TrustRoot,
    /// Held from the capacity count to the create, so two workspaces never
    /// both take the last of `max_containers`.
    creating: tokio::sync::Mutex<()>,
}

/// One container of a workspace, as the launcher reads it.
#[derive(Debug, Clone)]
struct Found {
    id: String,
    /// `None` when the generation label is missing or malformed.
    generation: Option<u64>,
    state: String,
    running: bool,
    image: Option<String>,
}

impl From<ContainerSummary> for Found {
    fn from(summary: ContainerSummary) -> Self {
        let generation = summary
            .labels
            .get(LABEL_GENERATION)
            .and_then(|text| parse_generation_label(text));
        Self {
            running: summary.is_running(),
            image: summary.labels.get(LABEL_IMAGE).cloned(),
            id: summary.id,
            generation,
            state: summary.state,
        }
    }
}

impl Service {
    /// `trust` is where checked directory walks start
    /// ([`TrustRoot::system`] outside tests).
    pub(crate) fn new(
        config: Arc<Config>,
        runtime: Arc<dyn RuntimeApi>,
        state: StateStore,
        trust: TrustRoot,
    ) -> Self {
        let dirs = SocketDirs::new(
            config.socket_host_dir.clone(),
            config.server_uid,
            config.server_gid,
            trust.clone(),
        );
        let names = Names::new(&config.name_prefix);
        Self {
            config,
            runtime,
            state,
            locks: WorkspaceLocks::default(),
            usage: UsageCache::default(),
            dirs,
            names,
            trust,
            creating: tokio::sync::Mutex::new(()),
        }
    }

    /// The state directory and every directory above it, checked again as
    /// at startup ([`TrustRoot::open_checked`]) before a container start
    /// records its generation there.
    fn check_state_dir(&self) -> Result<(), ServiceError> {
        let own_uid = agentkit_sys::effective_uid()
            .map_err(|error| ServiceError::Local(error.to_string()))?;
        self.trust
            .open_checked(&self.config.state_dir, own_uid)
            .map(drop)
            .map_err(|error| ServiceError::Local(format!("state directory refused: {error}")))
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
                    container_id: started.container_id,
                    channel_token: started.channel_token,
                }),
            Request::Stop {
                workspace_id,
                generation,
                grace_ms,
            } => self.stop(&workspace_id, generation, grace_ms).await,
            Request::Inspect { workspace_id } => self.inspect(&workspace_id).await,
            Request::Usage { workspace_id } => self.usage(&workspace_id).await,
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

    /// Count what the runtime holds at startup. Changes nothing.
    pub(crate) async fn reconcile(&self) -> Result<String, ServiceError> {
        let all = self.runtime.list_containers(&managed_filter()).await?;
        let running = all.iter().filter(|c| c.is_running()).count();
        Ok(format!(
            "{} managed containers ({running} running), {} workspace records",
            all.len(),
            self.state.len().await
        ))
    }

    async fn inspect(&self, ws: &str) -> Result<Reply, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let found = self.containers(&ws).await?;
        let fence = self.state.get(&ws).await;
        let running = found
            .iter()
            .filter(|f| f.running)
            .filter_map(|f| f.generation)
            .max();
        Ok(Reply::Inspected {
            running,
            stopped_through: (fence.stopped_through > 0).then_some(fence.stopped_through),
        })
    }

    async fn usage(&self, ws: &str) -> Result<Reply, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let sizes = self.usage.sizes(self.runtime.as_ref()).await?;
        let size = |name: String| match sizes.get(&name) {
            None => Ok(0),
            Some(Some(bytes)) => Ok(*bytes),
            Some(None) => Err(ServiceError::Runtime(RuntimeError::Malformed(format!(
                "the runtime reports no size for volume {name}"
            )))),
        };
        Ok(Reply::Usage {
            workspace_bytes: size(self.names.workspace_volume(&ws))?,
            home_bytes: size(self.names.home_volume(&ws))?,
        })
    }

    /// Every container labelled with `ws`, re-checked on this side.
    async fn containers(&self, ws: &WorkspaceId) -> Result<Vec<Found>, ServiceError> {
        let listed = self.runtime.list_containers(&workspace_filter(ws)).await?;
        Ok(listed
            .into_iter()
            .filter(|c| c.labels.get(LABEL_WORKSPACE).map(String::as_str) == Some(ws.as_str()))
            .map(Found::from)
            .collect())
    }
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
        Reply::Ensured { container_id, .. } => format!("ensured {}", short(container_id)),
        Reply::Stopped { proven, reason } => match reason {
            Some(reason) => format!("stopped proven={proven} ({reason})"),
            None => format!("stopped proven={proven}"),
        },
        Reply::Error(error) => format!("refused {:?}: {}", error.code, error.message),
        other => format!("{other:?}"),
    }
}

/// The first 12 characters of a container id, as the runtimes show it.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// One line on stderr. Names workspaces, generations and container ids;
/// never a request body or a full runtime answer.
pub(crate) fn log(message: &str) {
    eprintln!("agentkit-sandbox-launcher: {message}");
}

#[cfg(test)]
mod tests;
