//! `ensure`: start the workspace's container for a generation, or return
//! the running one.
//!
//! A generation runs at most once. The guest's operation table lives in
//! the supervisor's memory, so a generation that started again would
//! answer `unknown` for operations its first run executed, and the service
//! could run them twice. So each generation gets at most one container,
//! ever: `created_through` is recorded durably before the create, and
//!
//! - a container of the requested generation that is not `running`
//!   (exited, created and never started, paused, …) is refused with
//!   `busy`, and so is a generation already created whose container is
//!   gone (removed outside the launcher, or discarded after a failed
//!   start or a create with warnings). The service stops it (which proves
//!   the generation gone) and ensures the next generation;
//! - a start that fails after the launcher's own create removes the
//!   container again;
//! - a generation at or below one proven stopped, or below one a
//!   container was ever created for, is `stale_generation`, as is one
//!   below a container that still exists;
//! - a container of a lower generation, or of an unreadable one, is
//!   `busy`: never replaced, the service stops it first.
//!
//! Each created container gets a fresh channel token in its environment,
//! answered in `ensured`. An `ensure` of the running generation answers
//! the token read back from the container's environment; a running
//! container without a valid one is `busy` (the service stops it and moves
//! on), so `ensure` stays idempotent without the launcher storing tokens.
//!
//! A create is refused (`runtime_failed`, nothing created or recorded)
//! while the launcher holds `max_containers` containers in any state, and
//! when the state directory or the socket host directory (or one above
//! either) no longer passes the startup checks.

use agentkit_sandbox_proto::launcher::Tier;

use super::{Found, Service, log};
use crate::error::{ServiceError, bounded};
use crate::naming::{LABEL_MANAGED, WorkspaceId, check_generation, managed_filter};
use crate::runtime::RuntimeError;
use crate::state::Fence;
use crate::template::{Machine, container_body};
use crate::token::{new_token, token_of};

/// What `ensure` answers: the container and its channel token. No
/// `Debug`: the token is never printed.
pub(crate) struct Started {
    pub(crate) container_id: String,
    pub(crate) channel_token: String,
}

/// What `ensure` does once the checks passed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The generation runs: answer its container.
    Existing(String),
    /// Nothing of the workspace exists: create and start.
    Create,
}

impl Service {
    pub(super) async fn ensure(
        &self,
        ws: &str,
        generation: u64,
        image_digest: &str,
        tier: &Tier,
    ) -> Result<Started, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let generation = check_generation(generation)?;
        if image_digest != self.config.image_digest {
            return Err(ServiceError::ImageMismatch(
                "the requested image digest is not the launcher's".into(),
            ));
        }
        if !self.config.allows_tier(tier) {
            return Err(ServiceError::Invalid(
                "the tier is not one of the configured tiers".into(),
            ));
        }
        let _guard = self.locks.lock(&ws).await;
        let fence = self.state.get(&ws).await;
        let found = self.containers(&ws).await?;
        match decide(generation, fence, &found, &self.config.image_digest)? {
            Plan::Existing(id) => self.running(id).await,
            Plan::Create => self.create(&ws, generation, tier, fence).await,
        }
    }

    /// The running container of the requested generation, with the token
    /// it was created with.
    async fn running(&self, id: String) -> Result<Started, ServiceError> {
        let env = self.runtime.container_env(&id).await?;
        match token_of(&env) {
            Some(channel_token) => Ok(Started {
                container_id: id,
                channel_token,
            }),
            None => Err(ServiceError::Busy(
                "the running container carries no valid channel token; stop it".into(),
            )),
        }
    }

    async fn create(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        tier: &Tier,
        fence: Fence,
    ) -> Result<Started, ServiceError> {
        let slot = self.creating.lock().await;
        self.refuse_at_capacity().await?;
        self.check_state_dir()?;
        // Nothing of the workspace exists (see `decide`), so nothing mounts
        // the directory of the generation created last.
        let previous = fence.created_through;
        if previous > 0
            && let Err(error) = self.dirs.remove_generation(ws, previous)
        {
            log(&format!(
                "socket directory of {} g{previous} not removed: {error}",
                ws.as_str()
            ));
        }
        let socket_dir = self
            .dirs
            .prepare(ws, generation)
            .map_err(|error| ServiceError::Local(format!("socket directory refused: {error}")))?;
        self.ensure_volume(&self.names.workspace_volume(ws), ws, "workspace")
            .await?;
        self.ensure_volume(&self.names.home_volume(ws), ws, "home")
            .await?;
        let channel_token = new_token();
        let machine = Machine {
            ws,
            generation,
            tier,
            socket_dir: &socket_dir,
            channel_token: &channel_token,
        };
        let body = container_body(&self.config, machine);
        let name = self.names.container(ws, generation);
        // Durable before the create is sent: the generation's one container.
        self.state
            .record_created(ws, generation)
            .await
            .map_err(|error| ServiceError::Local(error.to_string()))?;
        let created = self.runtime.create_container(&name, &body).await?;
        // Listed from now on, so the next capacity count sees it.
        drop(slot);
        if !created.warnings.is_empty() {
            self.discard(&created.id).await;
            let warnings = bounded(&created.warnings.join("; "));
            return Err(RuntimeError::Refused(format!("create warned: {warnings}")).into());
        }
        if let Err(error) = self.runtime.start_container(&created.id).await {
            self.discard(&created.id).await;
            return Err(error.into());
        }
        Ok(Started {
            container_id: created.id,
            channel_token,
        })
    }

    /// Refuse a create while `max_containers` containers of the launcher
    /// exist, in any state.
    async fn refuse_at_capacity(&self) -> Result<(), ServiceError> {
        let listed = self.runtime.list_containers(&managed_filter()).await?;
        let held = listed
            .iter()
            .filter(|c| c.labels.get(LABEL_MANAGED).map(String::as_str) == Some("1"))
            .count();
        let max = self.config.max_containers;
        if held >= max {
            return Err(ServiceError::AtCapacity(format!(
                "the launcher holds its maximum of {max} containers; try again after a stop"
            )));
        }
        Ok(())
    }

    /// Remove a container the launcher just created and will not use.
    async fn discard(&self, id: &str) {
        if let Err(error) = self.runtime.remove_container(id).await {
            log(&format!("cannot remove unused container {id}: {error}"));
        }
    }
}

/// Decide from the fence and the workspace's containers.
fn decide(
    generation: u64,
    fence: Fence,
    found: &[Found],
    image_digest: &str,
) -> Result<Plan, ServiceError> {
    let highest = found.iter().filter_map(|f| f.generation).max().unwrap_or(0);
    let current = highest
        .max(fence.stopped_through)
        .max(fence.created_through);
    if generation <= fence.stopped_through
        || generation < fence.created_through
        || generation < highest
    {
        return Err(ServiceError::Stale {
            requested: generation,
            current,
        });
    }
    if let Some(other) = found.iter().find(|f| f.generation != Some(generation)) {
        return Err(other_generation(other));
    }
    same_generation(generation, fence, found, image_digest)
}

/// A container of another (lower or unreadable) generation blocks ensure.
fn other_generation(other: &Found) -> ServiceError {
    let which = other.generation.map_or_else(
        || "an unlabelled generation".to_string(),
        |g| format!("generation {g}"),
    );
    ServiceError::Busy(format!(
        "a container of {which} exists ({}); stop it first",
        other.state
    ))
}

/// Every container in `found` is of the requested generation.
fn same_generation(
    generation: u64,
    fence: Fence,
    found: &[Found],
    image_digest: &str,
) -> Result<Plan, ServiceError> {
    match found {
        [] if generation <= fence.created_through => Err(ServiceError::Busy(format!(
            "generation {generation} had a container already and cannot start again; stop it"
        ))),
        [] => Ok(Plan::Create),
        [same] if same.running => {
            if same.image.as_deref() != Some(image_digest) {
                return Err(ServiceError::ImageMismatch(
                    "the running container has another image".into(),
                ));
            }
            Ok(Plan::Existing(same.id.clone()))
        }
        [same] => Err(ServiceError::Busy(format!(
            "generation {generation} exists ({}) and cannot start again; stop it",
            same.state
        ))),
        _ => Err(ServiceError::Busy(format!(
            "more than one container of generation {generation} exists"
        ))),
    }
}
