//! `stop`: stop and remove every container of the workspace at or below a
//! generation, then prove them gone.
//!
//! `stopped_through` means no generation at or below it runs again, so a
//! stop of `G` covers every container of the workspace whose generation is
//! `G` or lower, and every one whose generation label is missing or
//! unreadable (the launcher cannot place it above `G`). Containers of a
//! higher generation are left alone.
//!
//! `proven: true` only when, after the removals, the runtime lists no
//! covered container of the workspace AND has no container of the name of
//! `G` or of any generation it removed, and the stopped-through record is
//! durable. A generation that never had a container is proven the same
//! way: the runtime's listing is the evidence, not the launcher's memory.
//! Anything less is `proven: false` with a reason, and nothing is
//! recorded.

use std::collections::BTreeSet;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::Reply;

use super::{Found, Service, log, short};
use crate::error::bounded;
use crate::naming::{WorkspaceId, check_generation};

/// Listings tried before a stop is answered unproven.
const PROOF_ATTEMPTS: u32 = 5;
/// Pause between listings (a forced removal can still be in progress).
#[cfg(not(test))]
const PROOF_INTERVAL: Duration = Duration::from_millis(400);
#[cfg(test)]
const PROOF_INTERVAL: Duration = Duration::from_millis(1);

/// True when a stop of `generation` must remove `found`.
fn covered(found: &Found, generation: u64) -> bool {
    found.generation.is_none_or(|g| g <= generation)
}

impl Service {
    pub(super) async fn stop(
        &self,
        ws: &str,
        generation: u64,
        grace_ms: u64,
    ) -> Result<Reply, crate::error::ServiceError> {
        let ws = WorkspaceId::parse(ws)?;
        let generation = check_generation(generation)?;
        let grace_secs = grace_ms.min(self.config.stop_grace_max_ms).div_ceil(1000);
        let _guard = self.locks.lock(&ws).await;
        let reply = match self.stop_locked(&ws, generation, grace_secs).await {
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
        grace_secs: u64,
    ) -> Result<(), String> {
        let found = self.containers(ws).await.map_err(|e| e.to_string())?;
        let mut generations = BTreeSet::from([generation]);
        for target in found.iter().filter(|f| covered(f, generation)) {
            generations.extend(target.generation);
            if let Err(error) = self.runtime.stop_container(&target.id, grace_secs).await {
                log(&format!("stop {} failed: {error}", short(&target.id)));
            }
            if let Err(error) = self.runtime.remove_container(&target.id).await {
                log(&format!("remove {} failed: {error}", short(&target.id)));
            }
        }
        self.prove_absent(ws, generation, &generations).await?;
        self.state
            .record_stopped(ws, generation)
            .await
            .map_err(|error| format!("gone, but the record was not saved: {error}"))?;
        for removed in generations {
            if let Err(error) = self.dirs.remove_generation(ws, removed) {
                log(&format!(
                    "socket directory of {} g{removed} not removed: {error}",
                    ws.as_str()
                ));
            }
        }
        Ok(())
    }

    async fn prove_absent(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        names: &BTreeSet<u64>,
    ) -> Result<(), String> {
        let mut reason = String::new();
        for attempt in 0..PROOF_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(PROOF_INTERVAL).await;
            }
            match self.absent_once(ws, generation, names).await {
                Ok(()) => return Ok(()),
                Err(why) => reason = why,
            }
        }
        Err(reason)
    }

    async fn absent_once(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        names: &BTreeSet<u64>,
    ) -> Result<(), String> {
        let found = self.containers(ws).await.map_err(|e| e.to_string())?;
        if let Some(left) = found.iter().find(|f| covered(f, generation)) {
            let which = left.generation.map_or_else(
                || "an unreadable generation".to_string(),
                |g| format!("generation {g}"),
            );
            return Err(format!(
                "container {} of {which} is still listed ({})",
                short(&left.id),
                left.state
            ));
        }
        for named in names {
            let name = self.names.container(ws, *named);
            match self.runtime.container_exists(&name).await {
                Ok(false) => {}
                Ok(true) => return Err(format!("container {name} still exists")),
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }
}
