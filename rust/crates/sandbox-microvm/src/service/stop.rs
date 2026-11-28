//! `stop`: stop the workspace's machine at or below a generation and prove
//! it gone.
//!
//! `stopped_through` means no generation at or below it runs again, so a
//! stop of `G` covers the held machine when its generation is `G` or
//! lower; a machine of a higher generation is left alone. Its helper's
//! stdin is closed (the helper stops its VM: 3 s for the guest, then a
//! forced stop, then it exits); after the request's grace it is
//! SIGKILLed, which ends the VM with it.
//!
//! `proven: true` only when the covered helper has exited and been reaped
//! (or none was held), no process has the workspace's disks open any more
//! (polled for up to [`crate::disks::RELEASE_WAIT`]: a helper's exit,
//! whatever its status, does not prove that Apple's VM service process let
//! go of the VM), and the stopped-through record, which also drops the
//! helper record, is durable. Anything less is `proven: false` with a
//! reason; the machine then stays held, so a later stop retries. While a
//! machine of a higher generation is held the disks are its own: it was
//! started only once every earlier VM had released them, so they are not
//! checked.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::Reply;
use tokio::task::JoinSet;

use super::{Service, log};
use crate::error::{ServiceError, VmShutdownError, bounded};
use crate::naming::{WorkspaceId, check_generation};

impl Service {
    pub(super) async fn stop(
        &self,
        ws: &str,
        generation: u64,
        grace_ms: u64,
    ) -> Result<Reply, ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let generation = check_generation(generation)?;
        let grace = Duration::from_millis(grace_ms.min(self.config.stop_grace_max_ms));
        let _guard = self.locks.lock(&ws).await;
        let reply = match self.stop_locked(&ws, generation, grace).await {
            Ok(()) => Reply::Stopped {
                proven: true,
                reason: None,
            },
            Err(reason) => Reply::Stopped {
                proven: false,
                reason: Some(bounded(&reason)),
            },
        };
        Ok(reply)
    }

    async fn stop_locked(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        grace: Duration,
    ) -> Result<(), String> {
        let mut covered = BTreeSet::from([generation]);
        let machine = self.machine(ws);
        let newer = machine.as_ref().is_some_and(|m| m.generation > generation);
        let held = machine.filter(|m| m.generation <= generation);
        let mut ended = None;
        if let Some(machine) = &held {
            covered.insert(machine.generation);
            ended = Some(machine.stop(grace).await?);
        }
        if !newer {
            self.disks_released(ws).await.map_err(|why| match ended {
                Some(exit) => format!("the helper ended ({exit}), but {why}"),
                None => why,
            })?;
        }
        if let Some(machine) = &held {
            self.remove(ws, machine);
        }
        self.state
            .record_stopped(ws.as_str(), generation)
            .await
            .map_err(|error| format!("gone, but the record was not saved: {error}"))?;
        for stopped in covered {
            if let Err(error) = self.sockets.remove(ws.as_str(), stopped) {
                log(&format!(
                    "socket directory of {} g{stopped} not removed: {error}",
                    ws.as_str()
                ));
            }
        }
        Ok(())
    }

    /// Wait, off the runtime, until no process holds the disks of `ws`.
    pub(super) async fn disks_released(&self, ws: &WorkspaceId) -> Result<(), String> {
        let (disks, ws, within) = (self.disks.clone(), ws.clone(), self.release_wait);
        tokio::task::spawn_blocking(move || disks.wait_released(&ws, within))
            .await
            .map_err(|error| format!("the disk check failed: {error}"))?
    }

    /// For a shutdown: refuse every request from now on, then stop every
    /// held machine at once with the longest grace.
    pub(crate) async fn stop_all(self: &Arc<Self>) -> Result<(), VmShutdownError> {
        let grace = self.config.stop_grace_max_ms;
        let held = self.close_and_held();
        let mut stops = JoinSet::new();
        for (ws, generation) in held.iter().cloned() {
            let service = Arc::clone(self);
            stops.spawn(async move {
                let reply = service.stop(&ws, generation, grace).await;
                matches!(reply, Ok(Reply::Stopped { proven: true, .. })).then_some(ws)
            });
        }
        let mut proven = HashSet::new();
        while let Some(joined) = stops.join_next().await {
            if let Ok(Some(ws)) = joined {
                proven.insert(ws);
            }
        }
        let mut unproven: Vec<String> = held
            .into_iter()
            .map(|(ws, _)| ws)
            .filter(|ws| !proven.contains(ws))
            .collect();
        unproven.sort();
        if unproven.is_empty() {
            Ok(())
        } else {
            Err(VmShutdownError::Unproven(unproven))
        }
    }
}
