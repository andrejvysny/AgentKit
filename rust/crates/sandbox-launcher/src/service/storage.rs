//! The workspace's two named volumes: created on first `ensure`,
//! recreated by `reset_home`, removed by `destroy`. Both of those refuse
//! while any container of the workspace exists, in any state. `destroy`
//! keeps the generation fence (see [`crate::state`]) and removes the
//! machine directories still left: a stop removes the directories of the
//! generations it covers and a create the one of the generation created
//! before, so only the last created or stopped generation's can remain.

use super::Service;
use crate::error::ServiceError;
use crate::naming::{LABEL_MANAGED, LABEL_VOLUME, LABEL_WORKSPACE, WorkspaceId, volume_labels};
use crate::runtime::{Removal, RuntimeError, VolumeInfo};

impl Service {
    pub(super) async fn reset_home(&self, ws: &str) -> Result<(), ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let _guard = self.locks.lock(&ws).await;
        self.refuse_if_any_container(&ws).await?;
        let name = self.names.home_volume(&ws);
        let removed = self.remove_volume(&name).await;
        self.usage.invalidate().await;
        removed?;
        self.runtime
            .create_volume(&name, &volume_labels(&ws, "home"))
            .await?;
        Ok(())
    }

    pub(super) async fn destroy(&self, ws: &str) -> Result<(), ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let _guard = self.locks.lock(&ws).await;
        self.refuse_if_any_container(&ws).await?;
        let workspace = self.remove_volume(&self.names.workspace_volume(&ws)).await;
        let home = self.remove_volume(&self.names.home_volume(&ws)).await;
        self.usage.invalidate().await;
        workspace?;
        home?;
        let fence = self.state.get(&ws).await;
        let generations = [fence.created_through, fence.stopped_through];
        for generation in generations.into_iter().filter(|g| *g > 0) {
            self.dirs
                .remove_generation(&ws, generation)
                .map_err(|error| {
                    ServiceError::Local(format!("socket directory not removed: {error}"))
                })?;
        }
        Ok(())
    }

    /// Create a volume of the workspace, or accept the existing one only
    /// when it is a plain `local` volume the launcher labelled for this
    /// workspace and role. A `local` volume with driver options can be a
    /// bind mount of any host path, so it is refused.
    pub(super) async fn ensure_volume(
        &self,
        name: &str,
        ws: &WorkspaceId,
        role: &str,
    ) -> Result<(), ServiceError> {
        match self.runtime.inspect_volume(name).await? {
            None => Ok(self
                .runtime
                .create_volume(name, &volume_labels(ws, role))
                .await?),
            Some(volume) if launcher_volume(&volume, ws, role) => Ok(()),
            Some(_) => Err(RuntimeError::Refused(format!(
                "volume {name} exists but is not a plain launcher volume of this workspace"
            ))
            .into()),
        }
    }

    async fn refuse_if_any_container(&self, ws: &WorkspaceId) -> Result<(), ServiceError> {
        let found = self.containers(ws).await?;
        match found.first() {
            None => Ok(()),
            Some(container) => Err(ServiceError::Busy(format!(
                "{} container(s) of the workspace exist (first: {}); stop them first",
                found.len(),
                container.state
            ))),
        }
    }

    async fn remove_volume(&self, name: &str) -> Result<(), ServiceError> {
        match self.runtime.remove_volume(name).await? {
            Removal::Removed | Removal::Absent => Ok(()),
            Removal::InUse => Err(ServiceError::Busy(format!(
                "volume {name} is in use by a container"
            ))),
        }
    }
}

fn launcher_volume(volume: &VolumeInfo, ws: &WorkspaceId, role: &str) -> bool {
    let label = |key: &str| volume.labels.get(key).map(String::as_str);
    volume.driver == "local"
        && volume.options.is_empty()
        && label(LABEL_MANAGED) == Some("1")
        && label(LABEL_WORKSPACE) == Some(ws.as_str())
        && label(LABEL_VOLUME) == Some(role)
}
