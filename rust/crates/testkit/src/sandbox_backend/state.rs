//! The fake's whole state, behind one mutex: workspace volumes, machines
//! by `(workspace, generation)`, scripts, armed faults and counters.

use std::collections::HashMap;

use agentkit_workspaces::{
    BackendDescriptor, Denial, Generation, MachineHandle, MachineSpec, NetworkPolicy, OperationId,
    PathRoot, ResourceLimits, SandboxError, StopProof, StorageUsage, WorkspaceId,
};

use super::guest::{Env, FakeOp};
use super::program::Program;
use super::volume::Volume;

const GIB: u64 = 1024 * 1024 * 1024;

pub(super) type MachineKey = (WorkspaceId, Generation);

/// Faults armed for the next new dispatch (any workspace), and the stop
/// refusal, which holds until switched off.
#[derive(Debug, Default)]
pub(super) struct Faults {
    pub(super) lose_next_ack: bool,
    pub(super) hang_next_dispatch: bool,
    pub(super) panic_next_dispatch: bool,
    pub(super) die_after_next_dispatch: bool,
    pub(super) endless_next_exec: bool,
    pub(super) refuse_stop: bool,
    /// A stop of a generation with no machine record answers `Stopped`
    /// (verified absence) instead of `Unproven`.
    pub(super) prove_absence: bool,
}

#[derive(Debug, Default)]
pub(super) struct Counters {
    pub(super) dispatch_calls: HashMap<OperationId, u32>,
    pub(super) machines_started: u32,
    pub(super) stop_calls: u32,
    pub(super) background_ended: u32,
}

pub(super) struct WorkspaceRec {
    pub(super) served: Generation,
    /// The highest generation a stop answered `Stopped` for: no generation
    /// at or below it starts again.
    pub(super) proven: Option<Generation>,
    pub(super) quota_bytes: u64,
    /// `/workspace`.
    pub(super) volume: Volume,
    /// `/home/agent`, on the same volume; `reset_home` empties it.
    pub(super) home: Volume,
    pub(super) disk_full: bool,
    pub(super) home_resets: u32,
}

impl WorkspaceRec {
    pub(super) fn new(served: Generation, quota_bytes: u64) -> Self {
        Self {
            served,
            proven: None,
            quota_bytes,
            volume: Volume::default(),
            home: Volume::default(),
            disk_full: false,
            home_resets: 0,
        }
    }

    /// The tree a request names.
    pub(super) fn tree(&self, root: PathRoot) -> &Volume {
        match root {
            PathRoot::Workspace => &self.volume,
            PathRoot::Home => &self.home,
        }
    }

    /// Bytes in use across both trees.
    pub(super) fn used_bytes(&self) -> u64 {
        self.volume.used_bytes() + self.home.used_bytes()
    }
}

pub(super) struct Machine {
    pub(super) opaque: String,
    pub(super) policy: NetworkPolicy,
    pub(super) ops: Vec<FakeOp>,
    pub(super) denials: Vec<Denial>,
    pub(super) stopped: bool,
    pub(super) unreachable: bool,
}

impl Machine {
    pub(super) fn op(&self, id: &OperationId) -> Option<&FakeOp> {
        self.ops.iter().find(|op| &op.id == id)
    }
}

pub(super) struct State {
    pub(super) descriptor: BackendDescriptor,
    pub(super) scripts: HashMap<String, Program>,
    pub(super) workspaces: HashMap<WorkspaceId, WorkspaceRec>,
    pub(super) machines: HashMap<MachineKey, Machine>,
    pub(super) faults: Faults,
    pub(super) counters: Counters,
    pub(super) observe_max_bytes: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            descriptor: BackendDescriptor {
                kind: "fake".to_owned(),
                supports_status_after_host_loss: false,
                enforces_disk_quota: false,
            },
            scripts: HashMap::new(),
            workspaces: HashMap::new(),
            machines: HashMap::new(),
            faults: Faults::default(),
            counters: Counters::default(),
            observe_max_bytes: 64 * 1024,
        }
    }
}

pub(super) fn quota_of(limits: &ResourceLimits) -> u64 {
    limits.disk_gib.saturating_mul(GIB)
}

fn unreachable_error(ws: &WorkspaceId, generation: Generation) -> SandboxError {
    SandboxError::Backend {
        reason: format!("machine of {ws} generation {generation} is unreachable"),
    }
}

fn missing(ws: &WorkspaceId, generation: Generation) -> SandboxError {
    SandboxError::NotFound {
        what: format!("machine of {ws} generation {generation}"),
    }
}

fn handle(ws: &WorkspaceId, generation: Generation, machine: &Machine) -> MachineHandle {
    MachineHandle {
        workspace_id: ws.clone(),
        generation,
        opaque: machine.opaque.clone(),
    }
}

impl State {
    pub(super) fn ensure(&mut self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        let (ws, generation) = (&spec.workspace_id, spec.generation);
        let served = self.workspaces.get(ws).map(|rec| rec.served);
        if let Some(current) = served.filter(|current| generation < *current) {
            return Err(SandboxError::StaleGeneration {
                requested: generation,
                current,
            });
        }
        let proven = self.workspaces.get(ws).and_then(|rec| rec.proven);
        if proven.is_some_and(|proven| generation <= proven) {
            // A generation proven stopped never starts, started or not.
            return Err(SandboxError::StaleGeneration {
                requested: generation,
                current: served.unwrap_or(generation).max(generation),
            });
        }
        if let Some(machine) = self.machines.get(&(ws.clone(), generation)) {
            if machine.stopped {
                // A proven-stopped generation never runs again.
                return Err(SandboxError::StaleGeneration {
                    requested: generation,
                    current: served.unwrap_or(generation),
                });
            }
            if machine.unreachable {
                return Err(unreachable_error(ws, generation));
            }
            return Ok(handle(ws, generation, machine));
        }
        if let Some(live) = self.live_generation(ws) {
            return Err(SandboxError::Backend {
                reason: format!("{ws} generation {live} still runs; stop it first"),
            });
        }
        Ok(self.start(spec))
    }

    fn start(&mut self, spec: &MachineSpec) -> MachineHandle {
        let ws = &spec.workspace_id;
        let rec = self
            .workspaces
            .entry(ws.clone())
            .or_insert_with(|| WorkspaceRec::new(spec.generation, 0));
        rec.served = spec.generation;
        rec.quota_bytes = quota_of(&spec.limits);
        self.counters.machines_started += 1;
        let machine = Machine {
            opaque: format!(
                "fake-{ws}-{}-{}",
                spec.generation, self.counters.machines_started
            ),
            policy: spec.network.clone(),
            ops: Vec::new(),
            denials: Vec::new(),
            stopped: false,
            unreachable: false,
        };
        let handle = handle(ws, spec.generation, &machine);
        self.machines.insert((ws.clone(), spec.generation), machine);
        handle
    }

    /// The generation of `ws` whose machine has not been stopped.
    pub(super) fn live_generation(&self, ws: &WorkspaceId) -> Option<Generation> {
        self.machines
            .iter()
            .find(|((owner, _), machine)| owner == ws && !machine.stopped)
            .map(|((_, generation), _)| *generation)
    }

    /// The live machine `m` names, after its operations caught up with
    /// `now`. Refuses an older generation (INV-8), a stopped or unknown
    /// machine, and an unreachable one.
    pub(super) fn machine(
        &mut self,
        m: &MachineHandle,
        now: i64,
    ) -> Result<&mut Machine, SandboxError> {
        let (ws, generation) = (&m.workspace_id, m.generation);
        let served = self
            .workspaces
            .get(ws)
            .map(|rec| rec.served)
            .ok_or_else(|| missing(ws, generation))?;
        if generation < served {
            return Err(SandboxError::StaleGeneration {
                requested: generation,
                current: served,
            });
        }
        let key = (ws.clone(), generation);
        match self.machines.get(&key) {
            Some(machine) if !machine.stopped && machine.opaque == m.opaque => {
                if machine.unreachable {
                    return Err(unreachable_error(ws, generation));
                }
            }
            _ => return Err(missing(ws, generation)),
        }
        self.step(&key, now);
        Ok(self.machines.get_mut(&key).expect("checked above"))
    }

    /// Move every operation of the machine at `key` on to `now`.
    fn step(&mut self, key: &MachineKey, now: i64) {
        let (Some(machine), Some(rec)) =
            (self.machines.get_mut(key), self.workspaces.get_mut(&key.0))
        else {
            return;
        };
        let Machine {
            ops,
            policy,
            denials,
            ..
        } = machine;
        let mut env = Env {
            volume: &mut rec.volume,
            home: &mut rec.home,
            quota: rec.quota_bytes,
            disk_full: rec.disk_full,
            policy,
            denials,
            background_ended: &mut self.counters.background_ended,
            now,
        };
        for op in ops.iter_mut() {
            op.step(&mut env);
        }
    }

    /// End `id` on the machine `m` names; an unknown id is left alone.
    pub(super) fn cancel(
        &mut self,
        m: &MachineHandle,
        id: &OperationId,
        now: i64,
    ) -> Result<(), SandboxError> {
        self.machine(m, now)?;
        let key = (m.workspace_id.clone(), m.generation);
        let machine = self.machines.get_mut(&key).expect("checked above");
        if let Some(op) = machine.ops.iter_mut().find(|op| &op.id == id) {
            op.cancel(&mut self.counters.background_ended);
        }
        Ok(())
    }

    /// Move the live, reachable machine of `ws` on to `now`, so what its
    /// programs did before a stop or a usage read has landed.
    pub(super) fn catch_up(&mut self, ws: &WorkspaceId, now: i64) {
        let Some(generation) = self.live_generation(ws) else {
            return;
        };
        let key = (ws.clone(), generation);
        if self.machines.get(&key).is_some_and(|m| !m.unreachable) {
            self.step(&key, now);
        }
    }

    pub(super) fn stop(&mut self, ws: &WorkspaceId, generation: Generation, now: i64) -> StopProof {
        self.counters.stop_calls += 1;
        self.catch_up(ws, now);
        if self.faults.refuse_stop {
            return StopProof::Unproven {
                reason: "fake fault: the stop is refused".to_owned(),
            };
        }
        match self.machines.get_mut(&(ws.clone(), generation)) {
            Some(machine) => machine.stopped = true,
            None if self.faults.prove_absence => {}
            None => {
                return StopProof::Unproven {
                    reason: format!("no record of a machine for {ws} generation {generation}"),
                };
            }
        }
        let rec = self.workspaces.entry(ws.clone()).or_insert_with(|| {
            WorkspaceRec::new(Generation::FIRST, quota_of(&ResourceLimits::DEFAULT))
        });
        rec.proven = rec.proven.max(Some(generation));
        StopProof::Stopped
    }

    /// Refuses while a machine of `ws` runs.
    pub(super) fn require_stopped(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        match self.live_generation(ws) {
            Some(live) => Err(SandboxError::Backend {
                reason: format!("{ws} generation {live} still runs; stop it first"),
            }),
            None => Ok(()),
        }
    }

    pub(super) fn usage(&self, ws: &WorkspaceId) -> StorageUsage {
        match self.workspaces.get(ws) {
            Some(rec) if rec.disk_full => StorageUsage {
                used_bytes: rec.quota_bytes,
                quota_bytes: rec.quota_bytes,
            },
            Some(rec) => StorageUsage {
                used_bytes: rec.used_bytes(),
                quota_bytes: rec.quota_bytes,
            },
            None => StorageUsage {
                used_bytes: 0,
                quota_bytes: quota_of(&ResourceLimits::DEFAULT),
            },
        }
    }
}
