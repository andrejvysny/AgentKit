//! The machines this service instance started and has not stopped: the
//! active-machine cap, idle eviction, live policy changes and the denial
//! cursor. In memory only: it is a cache of handles, never the truth (the
//! generation in the repository is). After a restart it starts empty.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::ids::{Generation, WorkspaceId};
use crate::machine::MachineHandle;

struct Live {
    handle: MachineHandle,
    last_used_at: i64,
    denial_cursor: u64,
}

/// One entry per workspace with a machine this instance started.
#[derive(Default)]
pub(super) struct Machines {
    live: Mutex<HashMap<WorkspaceId, Live>>,
}

impl Machines {
    fn lock(&self) -> MutexGuard<'_, HashMap<WorkspaceId, Live>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record a handle `ensure_machine` returned. A new generation resets
    /// the denial cursor. A handle older than the one recorded is ignored:
    /// a slow start of a superseded generation never replaces a newer one.
    pub(super) fn put(&self, handle: &MachineHandle, now: i64) {
        let mut live = self.lock();
        if let Some(entry) = live.get_mut(&handle.workspace_id) {
            if entry.handle == *handle {
                entry.last_used_at = entry.last_used_at.max(now);
                return;
            }
            if entry.handle.generation > handle.generation {
                return;
            }
        }
        let entry = Live {
            handle: handle.clone(),
            last_used_at: now,
            denial_cursor: 0,
        };
        live.insert(handle.workspace_id.clone(), entry);
    }

    /// The handle of `ws`, if this instance runs one.
    pub(super) fn handle(&self, ws: &WorkspaceId) -> Option<MachineHandle> {
        self.lock().get(ws).map(|entry| entry.handle.clone())
    }

    /// True when this instance started `(ws, generation)` and has not
    /// stopped it: evidence that a machine of that generation ran.
    pub(super) fn runs(&self, ws: &WorkspaceId, generation: Generation) -> bool {
        self.lock()
            .get(ws)
            .is_some_and(|entry| entry.handle.generation == generation)
    }

    /// Mark `ws` used at `now`.
    pub(super) fn touch(&self, ws: &WorkspaceId, now: i64) {
        if let Some(entry) = self.lock().get_mut(ws) {
            entry.last_used_at = entry.last_used_at.max(now);
        }
    }

    /// Forget `ws` (stopped, fenced or deleted).
    pub(super) fn forget(&self, ws: &WorkspaceId) {
        self.lock().remove(ws);
    }

    /// Forget `handle`'s machine when it is the one recorded.
    pub(super) fn forget_handle(&self, handle: &MachineHandle) {
        let mut live = self.lock();
        if live
            .get(&handle.workspace_id)
            .is_some_and(|entry| entry.handle == *handle)
        {
            live.remove(&handle.workspace_id);
        }
    }

    /// How many machines this instance runs.
    pub(super) fn count(&self) -> usize {
        self.lock().len()
    }

    /// Workspaces with a machine, least recently used first, except `skip`.
    pub(super) fn least_recently_used(&self, skip: Option<&WorkspaceId>) -> Vec<WorkspaceId> {
        let live = self.lock();
        let mut order: Vec<(i64, WorkspaceId)> = live
            .iter()
            .filter(|(ws, _)| Some(*ws) != skip)
            .map(|(ws, entry)| (entry.last_used_at, ws.clone()))
            .collect();
        order.sort();
        order.into_iter().map(|(_, ws)| ws).collect()
    }

    /// Workspaces whose machine was last used at or before `cutoff`, least
    /// recently used first.
    pub(super) fn idle_since(&self, cutoff: i64) -> Vec<WorkspaceId> {
        let live = self.lock();
        let mut order: Vec<(i64, WorkspaceId)> = live
            .iter()
            .filter(|(_, entry)| entry.last_used_at <= cutoff)
            .map(|(ws, entry)| (entry.last_used_at, ws.clone()))
            .collect();
        order.sort();
        order.into_iter().map(|(_, ws)| ws).collect()
    }

    /// The denial cursor of `handle`'s machine (0 for an unknown one).
    pub(super) fn denial_cursor(&self, handle: &MachineHandle) -> u64 {
        self.lock()
            .get(&handle.workspace_id)
            .filter(|entry| entry.handle == *handle)
            .map_or(0, |entry| entry.denial_cursor)
    }

    /// Move the denial cursor of `handle`'s machine forward.
    pub(super) fn set_denial_cursor(&self, handle: &MachineHandle, cursor: u64) {
        let mut live = self.lock();
        if let Some(entry) = live.get_mut(&handle.workspace_id)
            && entry.handle == *handle
        {
            entry.denial_cursor = entry.denial_cursor.max(cursor);
        }
    }
}
