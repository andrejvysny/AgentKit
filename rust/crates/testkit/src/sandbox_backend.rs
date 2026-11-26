//! `FakeSandboxBackend`: an in-memory `SandboxBackend` (ADR 0038) with
//! scripted programs and scripted faults. No process runs: a command's
//! shell text is looked up in a script table ([`FakeSandboxBackend::script`])
//! and its [`Program`] is played against a virtual clock.
//!
//! Time is a [`ManualClock`] the test advances, never the wall clock and
//! never tokio's paused time (the workspace's tokio has no `test-util`).
//! Progress is lazy: each port call that names a machine first moves that
//! machine's operations on to the clock's `now`. A test therefore steps a
//! program by advancing the clock and calling the port, deterministically.
//! Share one clock with the code under test through
//! [`FakeSandboxBackend::with_clock`].
//!
//! Machines are keyed by `(workspace, generation)`. The workspace volume
//! (files below `/workspace` and `/home/agent`, one quota for both) is
//! durable and outlives every generation;
//! a machine is disposable. Operations belong to the machine that ran
//! them: an id from another generation is `Unknown`.
//!
//! Faults armed with the `*_next_*` methods fire on the next dispatch
//! that starts a new operation (in any workspace), so a test can arm them
//! before the code under test runs; a replayed dispatch consumes none.
//! In every dispatch fault the operation has started before the fault
//! fires, as a real dispatch that may have reached the guest.

use std::sync::{Arc, Mutex, MutexGuard};

use agentkit_workspaces::{
    BackendDescriptor, FileMode, Generation, NetworkPolicy, OperationId, OperationState, RelPath,
    ResourceLimits, WorkspaceId,
};

use crate::ManualClock;

mod guest;
mod port;
mod program;
mod state;
mod volume;

pub use program::{CONNECT_REFUSED_EXIT, DISK_FULL_EXIT, Program, ProgramEnd, UNSCRIPTED_EXIT};

use state::{State, WorkspaceRec, quota_of};

/// Clones share one state and one clock.
#[derive(Clone)]
pub struct FakeSandboxBackend {
    state: Arc<Mutex<State>>,
    clock: Arc<ManualClock>,
}

impl Default for FakeSandboxBackend {
    fn default() -> Self {
        Self::with_clock(ManualClock::new(0))
    }
}

impl FakeSandboxBackend {
    /// A fake with its own clock at 0 ms and no scripts.
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake that reads `now` from `clock`.
    pub fn with_clock(clock: Arc<ManualClock>) -> Self {
        Self {
            state: Arc::default(),
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> i64 {
        agentkit_kernel::Runtime::now_ms(&*self.clock)
    }

    /// The clock the fake reads.
    pub fn clock(&self) -> Arc<ManualClock> {
        self.clock.clone()
    }

    /// Let `ms` of guest time pass. Programs move on at the next port call.
    pub fn advance(&self, ms: u64) {
        self.clock.advance(i64::try_from(ms).unwrap_or(i64::MAX));
    }

    /// Run `program` for every exec whose command text is exactly
    /// `command`. Unscripted text fails with [`UNSCRIPTED_EXIT`].
    pub fn script(&self, command: &str, program: Program) -> &Self {
        self.lock().scripts.insert(command.to_owned(), program);
        self
    }

    /// What `describe` answers (default: kind `fake`, both flags false).
    /// With `enforces_disk_quota`, a dispatch on a full disk answers
    /// `QuotaExceeded`.
    pub fn set_descriptor(&self, descriptor: BackendDescriptor) -> &Self {
        self.lock().descriptor = descriptor;
        self
    }

    /// Most output bytes one `observe` returns (default 64 KiB).
    pub fn set_observe_max_bytes(&self, max: u64) -> &Self {
        self.lock().observe_max_bytes = max;
        self
    }

    /// The next new dispatch starts the operation, then returns a backend
    /// error instead of the ack.
    pub fn lose_next_ack(&self) -> &Self {
        self.lock().faults.lose_next_ack = true;
        self
    }

    /// The next new dispatch starts the operation and never returns.
    pub fn hang_next_dispatch(&self) -> &Self {
        self.lock().faults.hang_next_dispatch = true;
        self
    }

    /// The next new dispatch starts the operation, then panics (INV-1).
    pub fn panic_next_dispatch(&self) -> &Self {
        self.lock().faults.panic_next_dispatch = true;
        self
    }

    /// The machine dies mid-run: after the next new dispatch acks, its
    /// machine is unreachable and every call naming it (and
    /// `ensure_machine` of its generation) is a backend error.
    /// `stop_machine` still proves the stop unless [`Self::refuse_stop`].
    pub fn die_after_next_dispatch(&self) -> &Self {
        self.lock().faults.die_after_next_dispatch = true;
        self
    }

    /// The live machine of `ws` dies now (see
    /// [`Self::die_after_next_dispatch`]).
    pub fn make_unreachable(&self, ws: &WorkspaceId) -> &Self {
        let mut state = self.lock();
        if let Some(generation) = state.live_generation(ws)
            && let Some(machine) = state.machines.get_mut(&(ws.clone(), generation))
        {
            machine.unreachable = true;
        }
        self
    }

    /// The next new exec prints without end, whatever its script says.
    pub fn endless_next_exec(&self) -> &Self {
        self.lock().faults.endless_next_exec = true;
        self
    }

    /// While `refuse` holds, `stop_machine` answers `Unproven` and the
    /// machine keeps running.
    pub fn refuse_stop(&self, refuse: bool) -> &Self {
        self.lock().faults.refuse_stop = refuse;
        self
    }

    /// While `prove` holds, a stop of a generation the fake has no machine
    /// record of answers `Stopped` (a backend that can verify absence)
    /// instead of `Unproven`. Either way a generation answered `Stopped`
    /// never starts again.
    pub fn prove_absence(&self, prove: bool) -> &Self {
        self.lock().faults.prove_absence = prove;
        self
    }

    /// Stale generation: the fake has served `generation` for `ws` (as
    /// after another host started it), so every older generation is
    /// refused with `StaleGeneration`.
    pub fn serve_generation(&self, ws: &WorkspaceId, generation: Generation) -> &Self {
        let mut state = self.lock();
        let rec = workspace_rec(&mut state, ws);
        rec.served = rec.served.max(generation);
        self
    }

    /// The disk of `ws` fills: `storage_usage` reports it exhausted,
    /// running commands and new writes fail with [`DISK_FULL_EXIT`], and
    /// with `enforces_disk_quota` a new dispatch answers `QuotaExceeded`.
    /// `destroy_storage` clears it.
    pub fn fill_disk(&self, ws: &WorkspaceId) -> &Self {
        let mut state = self.lock();
        workspace_rec(&mut state, ws).disk_full = true;
        self
    }

    /// How often `dispatch` was called with `id`, refused calls included.
    pub fn dispatch_calls(&self, id: &OperationId) -> u32 {
        let state = self.lock();
        state
            .counters
            .dispatch_calls
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    /// Calls to `dispatch`, all ids together.
    pub fn dispatch_calls_total(&self) -> u32 {
        self.lock().counters.dispatch_calls.values().sum()
    }

    /// Machines started so far.
    pub fn machines_started(&self) -> u32 {
        self.lock().counters.machines_started
    }

    /// Calls to `stop_machine`.
    pub fn stop_calls(&self) -> u32 {
        self.lock().counters.stop_calls
    }

    /// Machines not stopped (unreachable ones included), sorted.
    pub fn live_machines(&self) -> Vec<(WorkspaceId, Generation)> {
        let state = self.lock();
        let mut live: Vec<_> = state
            .machines
            .iter()
            .filter(|(_, machine)| !machine.stopped)
            .map(|(key, _)| key.clone())
            .collect();
        live.sort();
        live
    }

    /// The highest generation served for `ws`.
    pub fn served_generation(&self, ws: &WorkspaceId) -> Option<Generation> {
        self.lock().workspaces.get(ws).map(|rec| rec.served)
    }

    /// Background processes the fake ended with their command (F-7).
    pub fn background_ended(&self) -> u32 {
        self.lock().counters.background_ended
    }

    /// The recorded state of `id` on the machine of `(ws, generation)`,
    /// without moving it on; `None` when that machine never saw it.
    pub fn operation_state(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
        id: &OperationId,
    ) -> Option<OperationState> {
        let state = self.lock();
        let machine = state.machines.get(&(ws.clone(), generation))?;
        machine.op(id).map(|op| op.state)
    }

    /// The egress policy the machine of `(ws, generation)` enforces now;
    /// `None` when there is no such machine.
    pub fn machine_policy(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Option<NetworkPolicy> {
        let state = self.lock();
        let machine = state.machines.get(&(ws.clone(), generation))?;
        Some(machine.policy.clone())
    }

    /// Output bytes the guest holds for `id`, without moving it on.
    pub fn output_held(&self, ws: &WorkspaceId, generation: Generation, id: &OperationId) -> u64 {
        let state = self.lock();
        state
            .machines
            .get(&(ws.clone(), generation))
            .and_then(|machine| machine.op(id))
            .map_or(0, |op| op.output_len())
    }

    /// The content of a file below `/workspace`, read beside any machine.
    pub fn volume_file(&self, ws: &WorkspaceId, path: &str) -> Option<Vec<u8>> {
        let path = RelPath::parse(path).ok()?;
        let state = self.lock();
        let rec = state.workspaces.get(ws)?;
        rec.volume.file(&path).map(|file| file.bytes.clone())
    }

    /// The content of a file below `/home/agent`, read beside any machine.
    pub fn home_file(&self, ws: &WorkspaceId, path: &str) -> Option<Vec<u8>> {
        let path = RelPath::parse(path).ok()?;
        let state = self.lock();
        let rec = state.workspaces.get(ws)?;
        rec.home.file(&path).map(|file| file.bytes.clone())
    }

    /// The mode a volume file was written with.
    pub fn volume_file_mode(&self, ws: &WorkspaceId, path: &str) -> Option<FileMode> {
        let path = RelPath::parse(path).ok()?;
        let state = self.lock();
        let rec = state.workspaces.get(ws)?;
        rec.volume.file(&path).map(|file| file.mode)
    }

    /// Successful `reset_home` calls for `ws` (each empties `/home/agent`).
    pub fn home_resets(&self, ws: &WorkspaceId) -> u32 {
        self.lock()
            .workspaces
            .get(ws)
            .map_or(0, |rec| rec.home_resets)
    }
}

fn workspace_rec<'a>(state: &'a mut State, ws: &WorkspaceId) -> &'a mut WorkspaceRec {
    state
        .workspaces
        .entry(ws.clone())
        .or_insert_with(|| WorkspaceRec::new(Generation::FIRST, quota_of(&ResourceLimits::DEFAULT)))
}
