//! Per-workspace state: the generation served, the highest generation
//! proven stopped, the live machine and the last limits. Two locks: a
//! plain mutex over the state, never held across an `.await`, and one
//! async lifecycle lock per workspace that serializes `ensure_machine`,
//! `stop_machine` and the volume calls of that workspace. Calls on a
//! machine take only the first, briefly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agentkit_workspaces::{
    Generation, MachineHandle, MachineSpec, ResourceLimits, SandboxError, WorkspaceId,
};
use tokio::sync::OwnedMutexGuard;

use crate::machine::Machine;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct WsState {
    /// The highest generation the launcher started for this backend.
    served: Option<Generation>,
    /// The highest generation answered `Stopped`: none at or below it
    /// starts again (INV-5).
    stopped_through: Option<Generation>,
    machine: Option<Arc<Machine>>,
    limits: Option<ResourceLimits>,
}

/// What `ensure_machine` does for a spec.
pub(crate) enum Admission {
    /// The live machine of that generation.
    Live(MachineHandle),
    /// Nothing runs for it: start it.
    Start,
}

#[derive(Default)]
pub(crate) struct Registry {
    states: Mutex<HashMap<WorkspaceId, WsState>>,
    lifecycles: Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
}

impl Registry {
    /// The lifecycle lock of `ws`.
    pub(crate) async fn lifecycle(&self, ws: &WorkspaceId) -> OwnedMutexGuard<()> {
        let mutex = lock(&self.lifecycles)
            .entry(ws.clone())
            .or_default()
            .clone();
        mutex.lock_owned().await
    }

    fn with<T>(&self, ws: &WorkspaceId, f: impl FnOnce(&mut WsState) -> T) -> T {
        f(lock(&self.states).entry(ws.clone()).or_default())
    }

    /// The generation rules of `ensure_machine`, in the fake's order: a
    /// generation below the served one, or at or below a proven stop, is
    /// stale; the live machine of the generation is returned; another
    /// live generation must be stopped first.
    ///
    /// The live machine of the generation that is unreachable (its
    /// supervisor died, or broke the protocol) is stale at its own
    /// generation, as a `busy` launcher is (`mapping::launcher`): the
    /// service then proves it stopped and moves to the next generation.
    /// `Unavailable` would make it retry this generation until the
    /// machine is evicted.
    pub(crate) fn admit(&self, spec: &MachineSpec) -> Result<Admission, SandboxError> {
        let requested = spec.generation;
        self.with(&spec.workspace_id, |state| {
            if let Some(current) = state.served.filter(|served| requested < *served) {
                return Err(SandboxError::StaleGeneration { requested, current });
            }
            if state
                .stopped_through
                .is_some_and(|proven| requested <= proven)
            {
                let current = state
                    .served
                    .map_or(requested, |served| served.max(requested));
                return Err(SandboxError::StaleGeneration { requested, current });
            }
            match &state.machine {
                Some(machine) if machine.handle.generation == requested => {
                    match machine.unreachable() {
                        Some(_) => Err(SandboxError::StaleGeneration {
                            requested,
                            current: requested,
                        }),
                        None => Ok(Admission::Live(machine.handle.clone())),
                    }
                }
                Some(machine) => Err(SandboxError::Backend {
                    reason: format!(
                        "generation {} of {} still runs; stop it first",
                        machine.handle.generation, spec.workspace_id
                    ),
                }),
                None => Ok(Admission::Start),
            }
        })
    }

    /// The launcher started `generation`: later calls naming a lower one
    /// are stale.
    pub(crate) fn served(&self, ws: &WorkspaceId, generation: Generation, limits: ResourceLimits) {
        self.with(ws, |state| {
            state.served = state.served.max(Some(generation));
            state.limits = Some(limits);
        });
    }

    /// `generation` is proven stopped.
    pub(crate) fn proven(&self, ws: &WorkspaceId, generation: Generation) {
        self.with(ws, |state| {
            state.stopped_through = state.stopped_through.max(Some(generation));
        });
    }

    pub(crate) fn install(&self, machine: Arc<Machine>) {
        let ws = machine.handle.workspace_id.clone();
        self.with(&ws, |state| state.machine = Some(machine));
    }

    /// Remove and return the live machine when its generation is
    /// `generation` or lower: a stop of a generation covers every lower
    /// one (the launcher removes their containers too).
    pub(crate) fn take(&self, ws: &WorkspaceId, generation: Generation) -> Option<Arc<Machine>> {
        self.with(ws, |state| {
            let covered = state
                .machine
                .as_ref()
                .is_some_and(|machine| machine.handle.generation <= generation);
            if covered { state.machine.take() } else { None }
        })
    }

    /// The live machine of `ws`, reachable or not.
    pub(crate) fn live(&self, ws: &WorkspaceId) -> Option<Arc<Machine>> {
        self.with(ws, |state| state.machine.clone())
    }

    /// The quota of `ws`'s volume: from the last spec, else the default.
    pub(crate) fn quota_bytes(&self, ws: &WorkspaceId) -> u64 {
        let limits = self.with(ws, |state| state.limits.unwrap_or_default());
        limits.disk_gib.saturating_mul(1024 * 1024 * 1024)
    }

    /// The live, reachable machine `m` names. A generation below the
    /// served one is stale (INV-8); a handle that names no live machine,
    /// or an unreachable one, is unavailable.
    pub(crate) fn machine(&self, m: &MachineHandle) -> Result<Arc<Machine>, SandboxError> {
        let (served, machine) = self.with(&m.workspace_id, |state| {
            (state.served, state.machine.clone())
        });
        if let Some(current) = served.filter(|served| m.generation < *served) {
            return Err(SandboxError::StaleGeneration {
                requested: m.generation,
                current,
            });
        }
        match machine {
            Some(machine) if machine.handle == *m => match machine.unreachable() {
                Some(why) => Err(machine.unavailable(&why)),
                None => Ok(machine),
            },
            _ => Err(SandboxError::Unavailable {
                reason: format!(
                    "no machine of {} generation {} runs",
                    m.workspace_id, m.generation
                ),
            }),
        }
    }
}
