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
//! (or none was held) and the stopped-through record, which also drops the
//! helper record, is durable. Anything less is `proven: false` with a
//! reason; the machine then stays held, so a later stop retries.

use std::collections::BTreeSet;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::Reply;

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
        let held = self.machine(ws).filter(|m| m.generation <= generation);
        if let Some(machine) = held {
            covered.insert(machine.generation);
            machine.stop(grace).await?;
            self.remove(ws, &machine);
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

    /// Stop every held machine with the longest grace, for a shutdown.
    pub(crate) async fn stop_all(&self) -> Result<(), VmShutdownError> {
        let grace = self.config.stop_grace_max_ms;
        let mut unproven = Vec::new();
        for (ws, generation) in self.held() {
            match self.stop(&ws, generation, grace).await {
                Ok(Reply::Stopped { proven: true, .. }) => {}
                _ => unproven.push(ws),
            }
        }
        if unproven.is_empty() {
            Ok(())
        } else {
            Err(VmShutdownError::Unproven(unproven))
        }
    }
}
