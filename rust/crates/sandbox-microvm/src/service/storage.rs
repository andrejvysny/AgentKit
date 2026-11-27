//! `inspect`, `usage`, `reset_home` and `destroy`. The last two refuse
//! while a machine of the workspace is held, running or exited, until a
//! stop proved it gone. `destroy` keeps the generation record (see
//! [`crate::state`]) and removes the machine directories still left.

use agentkit_sandbox_proto::launcher::Reply;

use super::Service;
use crate::error::ServiceError;
use crate::naming::WorkspaceId;

impl Service {
    pub(super) async fn inspect(&self, ws: &str) -> Result<Reply, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let record = self.state.get(&ws).await;
        let running = self
            .machine(&ws)
            .filter(|machine| machine.running())
            .map(|machine| machine.generation);
        Ok(Reply::Inspected {
            running,
            stopped_through: (record.stopped_through > 0).then_some(record.stopped_through),
        })
    }

    /// Allocated bytes of both disks.
    pub(super) fn usage(&self, ws: &str) -> Result<Reply, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let (workspace_bytes, home_bytes) = self
            .disks
            .usage(&ws)
            .map_err(|error| ServiceError::Local(format!("disk usage unreadable: {error}")))?;
        Ok(Reply::Usage {
            workspace_bytes,
            home_bytes,
        })
    }

    pub(super) async fn reset_home(&self, ws: &str) -> Result<(), ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let _guard = self.locks.lock(&ws).await;
        self.refuse_if_held(&ws)?;
        self.disks
            .reset_home(&ws)
            .map_err(|error| ServiceError::Local(format!("home disk not reset: {error}")))
    }

    pub(super) async fn destroy(&self, ws: &str) -> Result<(), ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let _guard = self.locks.lock(&ws).await;
        self.refuse_if_held(&ws)?;
        self.disks
            .destroy(&ws)
            .map_err(|error| ServiceError::Local(format!("disks not removed: {error}")))?;
        let record = self.state.get(&ws).await;
        let generations = [record.created_through, record.stopped_through];
        for generation in generations.into_iter().filter(|g| *g > 0) {
            self.sockets
                .remove(ws.as_str(), generation)
                .map_err(|error| {
                    ServiceError::Local(format!("socket directory not removed: {error}"))
                })?;
        }
        Ok(())
    }

    fn refuse_if_held(&self, ws: &WorkspaceId) -> Result<(), ServiceError> {
        match self.machine(ws) {
            None => Ok(()),
            Some(machine) => {
                let state = if machine.running() { "runs" } else { "exited" };
                Err(ServiceError::Busy(format!(
                    "the machine of generation {} {state}; stop it first",
                    machine.generation
                )))
            }
        }
    }
}
