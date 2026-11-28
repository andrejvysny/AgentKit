//! `ensure`: start the workspace's machine for a generation, or answer the
//! running one.
//!
//! A generation runs at most once (the supervisor's operation table lives
//! in guest memory), so `created_through` is recorded durably before the
//! helper is spawned, and
//!
//! - a generation at or below one proven stopped, or below one a helper
//!   was ever started for (or below the held machine's), is
//!   `stale_generation`;
//! - a held machine of another generation is `busy`: never replaced, the
//!   service stops it first;
//! - a generation already started whose helper exited, failed to connect
//!   or is gone is `busy`: the service proves it stopped and moves on;
//! - the running generation answers its machine and its token again.
//!
//! A start is refused (`runtime_failed`, nothing recorded) while
//! `max_machines` helpers run, while a process still has the workspace's
//! disks open after [`crate::disks::RELEASE_WAIT`] (a VM that outlived its
//! helper; two VMs never share a disk), and when the kernel or root disk
//! changed since startup (`image_mismatch`). `ensured` is answered once
//! the helper reports `connected`; a helper that reports anything else, or
//! nothing within `connect_timeout`, is stopped and the generation stays
//! burnt.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::Tier;

use super::{Service, log, machine_id};
use crate::config::{CONTROL_SOCKET, EGRESS_SOCKET, Shape};
use crate::disks::{WorkspaceFiles, open_log};
use crate::error::ServiceError;
use crate::helper::{HELPER_PROTOCOL, HelperConfig, kernel_cmdline};
use crate::machine::Machine;
use crate::naming::{WorkspaceId, check_generation, new_token};
use crate::state::{HelperRecord, Record};

/// What `ensure` answers. No `Debug`: the token is never printed.
pub(crate) struct Started {
    pub(crate) machine_id: String,
    pub(crate) channel_token: String,
}

/// What `ensure` does once the checks passed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The generation runs: answer its machine.
    Existing,
    /// Nothing of the workspace is held: start a helper.
    Create,
}

/// The held machine as `decide` sees it.
#[derive(Debug, Clone, Copy)]
struct Held {
    generation: u64,
    running: bool,
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
        if image_digest != self.image_digest {
            return Err(ServiceError::ImageMismatch(
                "the requested image digest is not the launcher's".into(),
            ));
        }
        let shape = self.config.shape_of(tier).ok_or_else(|| {
            ServiceError::Invalid("the tier is not one of the configured tiers".into())
        })?;
        let _guard = self.locks.lock(&ws).await;
        let record = self.state.get(&ws).await;
        let live = self.machine(&ws);
        let held = live.as_ref().map(|m| Held {
            generation: m.generation,
            running: m.running(),
        });
        match (decide(generation, record, held)?, live) {
            (Plan::Existing, Some(machine)) => Ok(Started {
                machine_id: machine_id(&machine),
                channel_token: machine.token.clone(),
            }),
            (Plan::Create, _) => self.create(&ws, generation, shape, record).await,
            (Plan::Existing, None) => Err(ServiceError::Local("no machine is held".into())),
        }
    }

    async fn create(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        shape: Shape,
        record: Record,
    ) -> Result<Started, ServiceError> {
        self.disks_released(ws)
            .await
            .map_err(ServiceError::DisksHeld)?;
        let slot = self.creating.lock().await;
        self.refuse_at_capacity()?;
        self.images
            .check_unchanged()
            .map_err(ServiceError::ImageMismatch)?;
        let token = new_token();
        let (line, stderr) = self.prepare_files(ws, generation, shape, record, &token)?;
        // Durable before the spawn: the generation's one helper.
        self.state
            .record_created(ws, generation)
            .await
            .map_err(|error| ServiceError::Local(error.to_string()))?;
        let machine = Machine::spawn(&self.config.helper, stderr, generation, token)
            .map_err(|error| ServiceError::Helper(format!("the helper did not start: {error}")))?;
        let machine = Arc::new(machine);
        self.keep(ws, &machine).await?;
        drop(slot);
        match self.boot(&machine, &line).await {
            Ok(()) => Ok(Started {
                machine_id: machine_id(&machine),
                channel_token: machine.token.clone(),
            }),
            Err(why) => {
                self.discard(ws, &machine).await;
                Err(ServiceError::Helper(why))
            }
        }
    }

    /// The workspace's disks, a fresh machine socket directory, the
    /// helper's configuration line and its stderr log.
    fn prepare_files(
        &self,
        ws: &WorkspaceId,
        generation: u64,
        shape: Shape,
        record: Record,
        token: &str,
    ) -> Result<(String, std::fs::File), ServiceError> {
        let files = self
            .disks
            .prepare(ws)
            .map_err(|error| local("workspace disks", error))?;
        // Nothing of the workspace is held, so nobody uses the directory of
        // the generation started before.
        let previous = record.created_through;
        if previous > 0
            && let Err(error) = self.sockets.remove(ws.as_str(), previous)
        {
            log(&format!(
                "socket directory of {} g{previous} not removed: {error}",
                ws.as_str()
            ));
        }
        let socket_dir = self
            .sockets
            .prepare(ws, generation)
            .map_err(|error| local("socket directory", error))?;
        let line = self.helper_line(generation, shape, &files, &socket_dir, token)?;
        let stderr = open_log(&files.helper_log).map_err(|error| local("helper log", error))?;
        Ok((line, stderr))
    }

    /// Record the helper durably and hold it; a helper that cannot be
    /// recorded is killed at once, and one that a shutdown keeps from being
    /// held is stopped.
    async fn keep(&self, ws: &WorkspaceId, machine: &Arc<Machine>) -> Result<(), ServiceError> {
        let helper = HelperRecord {
            generation: machine.generation,
            pid: machine.pid,
            started: machine.started,
        };
        if let Err(error) = self.state.record_helper(ws, helper).await {
            if let Err(why) = machine.stop(Duration::ZERO).await {
                log(&format!(
                    "unrecorded helper of {} not stopped: {why}",
                    ws.as_str()
                ));
            }
            return Err(ServiceError::Local(error.to_string()));
        }
        if !self.insert(ws, Arc::clone(machine)) {
            self.discard(ws, machine).await;
            return Err(ServiceError::ShuttingDown);
        }
        Ok(())
    }

    async fn boot(&self, machine: &Machine, line: &str) -> Result<(), String> {
        machine.configure(line).await?;
        machine.wait_connected(self.config.connect_timeout).await
    }

    /// Stop a helper whose start failed. Once reaped and its disks released
    /// it is forgotten (its generation stays burnt); otherwise it stays held
    /// for a stop.
    async fn discard(&self, ws: &WorkspaceId, machine: &Arc<Machine>) {
        let stopped = match machine.stop(Duration::ZERO).await {
            Ok(_) => self.disks_released(ws).await,
            Err(why) => Err(why),
        };
        match stopped {
            Ok(()) => {
                self.remove(ws, machine);
                if let Err(error) = self.state.clear_helper(ws).await {
                    log(&format!(
                        "helper record of {} not cleared: {error}",
                        ws.as_str()
                    ));
                }
            }
            Err(why) => log(&format!(
                "failed helper of {} not proven stopped: {why}",
                ws.as_str()
            )),
        }
    }

    /// Refuse a start while `max_machines` helpers run.
    fn refuse_at_capacity(&self) -> Result<(), ServiceError> {
        let max = self.config.max_machines;
        if self.running_count() >= max {
            return Err(ServiceError::AtCapacity(format!(
                "the launcher runs its maximum of {max} machines; try again after a stop"
            )));
        }
        Ok(())
    }

    /// The helper's configuration line. It carries the token.
    fn helper_line(
        &self,
        generation: u64,
        shape: Shape,
        files: &WorkspaceFiles,
        socket_dir: &Path,
        token: &str,
    ) -> Result<String, ServiceError> {
        let config = HelperConfig {
            protocol: HELPER_PROTOCOL,
            kernel: utf8(&self.config.kernel.path)?,
            root_disk: utf8(&self.config.root_disk.path)?,
            workspace_disk: utf8(&files.workspace_disk)?,
            home_disk: utf8(&files.home_disk)?,
            cpus: shape.cpus,
            memory_bytes: shape.memory_bytes,
            cmdline: kernel_cmdline(generation, &self.image_digest, token),
            control_socket: utf8(&socket_dir.join(CONTROL_SOCKET))?,
            egress_socket: utf8(&socket_dir.join(EGRESS_SOCKET))?,
            console_log: utf8(&files.console_log)?,
        };
        serde_json::to_string(&config).map_err(|error| ServiceError::Local(error.to_string()))
    }
}

/// Decide from the record and the held machine.
fn decide(generation: u64, record: Record, held: Option<Held>) -> Result<Plan, ServiceError> {
    let highest = held.map_or(0, |held| held.generation);
    if generation <= record.stopped_through
        || generation < record.created_through
        || generation < highest
    {
        let current = highest
            .max(record.stopped_through)
            .max(record.created_through);
        return Err(ServiceError::Stale {
            requested: generation,
            current,
        });
    }
    match held {
        Some(held) if held.generation != generation => Err(ServiceError::Busy(format!(
            "a machine of generation {} is held; stop it first",
            held.generation
        ))),
        Some(held) if held.running => Ok(Plan::Existing),
        Some(_) => Err(ServiceError::Busy(format!(
            "the helper of generation {generation} exited and cannot start again; stop it"
        ))),
        None if generation <= record.created_through => Err(ServiceError::Busy(format!(
            "generation {generation} had a machine already and cannot start again; stop it"
        ))),
        None => Ok(Plan::Create),
    }
}

fn utf8(path: &Path) -> Result<String, ServiceError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ServiceError::Local(format!("{} is not UTF-8", path.display())))
}

fn local(what: &str, error: std::io::Error) -> ServiceError {
    ServiceError::Local(format!("{what} refused: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(stopped_through: u64, created_through: u64) -> Record {
        Record {
            stopped_through,
            created_through,
            helper: None,
        }
    }

    fn held(generation: u64, running: bool) -> Option<Held> {
        Some(Held {
            generation,
            running,
        })
    }

    #[test]
    fn generations_follow_the_launcher_rules() {
        let stale = |r| matches!(r, Err(ServiceError::Stale { .. }));
        let busy = |r| matches!(r, Err(ServiceError::Busy(_)));
        assert_eq!(decide(1, record(0, 0), None).unwrap(), Plan::Create);
        assert!(stale(decide(2, record(2, 2), None)), "proven stopped");
        assert!(stale(decide(1, record(0, 2), None)), "below one started");
        assert!(busy(decide(2, record(0, 2), None)), "started once already");
        assert_eq!(decide(3, record(2, 2), None).unwrap(), Plan::Create);
        assert_eq!(
            decide(3, record(0, 3), held(3, true)).unwrap(),
            Plan::Existing
        );
        assert!(busy(decide(3, record(0, 3), held(3, false))), "exited");
        assert!(busy(decide(4, record(0, 3), held(3, true))), "another held");
        assert!(stale(decide(2, record(0, 3), held(3, true))));
        match decide(1, record(4, 2), None) {
            Err(ServiceError::Stale { requested, current }) => {
                assert_eq!((requested, current), (1, 4));
            }
            _ => panic!("stale expected"),
        }
    }
}
