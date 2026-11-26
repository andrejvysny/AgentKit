//! One live machine: its handle, guest channel, egress proxy and the
//! operation book that keeps every receipt it has seen.
//!
//! The book holds the ids this machine acknowledged and the terminal
//! state first seen for each. A guest that later reports another state
//! for a settled id, forgets an acknowledged id, or starts one again
//! breaks INV-2/INV-7 and the channel is closed.
//!
//! It also holds a host-side deadline for each exec: the dispatch time
//! (the backend's clock) plus the exec's `timeout_ms` plus
//! [`DEADLINE_GRACE_MS`], set at its first dispatch only. The guest ends an
//! exec at its own deadline, so one still reported running after the host's
//! is a guest that lies (it shares the workload's uid), and would hold the
//! workspace's lease for ever.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use agentkit_egress::EgressProxy;
use agentkit_workspaces::{MachineHandle, OperationState, SandboxError};

use crate::channel::{Channel, ChannelError};
use crate::verify::Broken;

/// How long a stop waits for the egress proxy to close its tunnels.
const EGRESS_SHUTDOWN: Duration = Duration::from_secs(5);
/// Time past an exec's own timeout before the host stops believing the
/// guest that it still runs. The guest needs at most a few seconds (its
/// SIGTERM to SIGKILL grace, the reap) beyond the timeout.
pub(crate) const DEADLINE_GRACE_MS: u64 = 30_000;

/// Longest wait for the guest's answer to the cancel sent for an exec past
/// its deadline; the channel is closed after it whatever the answer.
pub(crate) const OVERDUE_CANCEL_WAIT: Duration = Duration::from_secs(2);

/// The backend's clock: epoch milliseconds from the kernel `Runtime` of
/// its egress ports.
pub(crate) type Now = Arc<dyn Fn() -> i64 + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Acknowledged operations and the one marked current for egress.
#[derive(Debug, Default)]
struct OpBook {
    /// Id -> terminal state, once one was seen.
    ops: HashMap<String, Option<OperationState>>,
    /// Exec id -> the host's deadline, epoch milliseconds.
    deadlines: HashMap<String, i64>,
    current: Option<String>,
}

/// See [`Machine::close_on_drop`].
pub(crate) struct CloseOnDrop<'a> {
    machine: &'a Machine,
    broken: Broken,
}

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.machine.violation(self.broken);
    }
}

/// A started, greeted machine.
pub(crate) struct Machine {
    pub(crate) handle: MachineHandle,
    pub(crate) channel: Channel,
    pub(crate) request_timeout: Duration,
    egress: Mutex<Option<EgressProxy>>,
    book: Mutex<OpBook>,
    control_path: PathBuf,
    now: Now,
}

impl Machine {
    pub(crate) fn new(
        handle: MachineHandle,
        channel: Channel,
        egress: EgressProxy,
        request_timeout: Duration,
        control_path: PathBuf,
        now: Now,
    ) -> Self {
        Self {
            handle,
            channel,
            request_timeout,
            egress: Mutex::new(Some(egress)),
            book: Mutex::new(OpBook::default()),
            control_path,
            now,
        }
    }

    /// The backend's clock, epoch milliseconds.
    pub(crate) fn now_ms(&self) -> i64 {
        (self.now)()
    }

    /// Set the host deadline of exec `op` dispatched at `dispatched_ms`
    /// with `timeout_ms`, unless its first dispatch set one already (a
    /// replay never extends it).
    pub(crate) fn set_deadline(&self, op: &str, dispatched_ms: i64, timeout_ms: u64) {
        let allowed = timeout_ms.saturating_add(DEADLINE_GRACE_MS);
        let deadline = dispatched_ms.saturating_add(i64::try_from(allowed).unwrap_or(i64::MAX));
        lock(&self.book)
            .deadlines
            .entry(op.to_owned())
            .or_insert(deadline);
    }

    /// True when exec `op` has a host deadline and it has passed.
    pub(crate) fn overdue(&self, op: &str) -> bool {
        let deadline = lock(&self.book).deadlines.get(op).copied();
        deadline.is_some_and(|deadline| self.now_ms() >= deadline)
    }

    /// True once the channel closed.
    pub(crate) fn unreachable(&self) -> Option<String> {
        self.channel.failure()
    }

    fn name(&self) -> String {
        format!(
            "machine of {} generation {}",
            self.handle.workspace_id, self.handle.generation
        )
    }

    /// The port error of an unreachable machine.
    pub(crate) fn unavailable(&self, why: &str) -> SandboxError {
        SandboxError::Unavailable {
            reason: format!("{} is unreachable: {why}", self.name()),
        }
    }

    /// The port error of a request that got no reply.
    pub(crate) fn channel_error(&self, error: ChannelError, what: &str) -> SandboxError {
        match error {
            ChannelError::Closed(why) => self.unavailable(&why),
            ChannelError::Timeout => SandboxError::Unavailable {
                reason: format!(
                    "{} did not answer {what} within {} ms",
                    self.name(),
                    self.request_timeout.as_millis()
                ),
            },
            ChannelError::TooLarge(why) => SandboxError::Backend {
                reason: format!("{what} does not fit one control message: {why}"),
            },
        }
    }

    /// A guard that closes the channel for `broken` when dropped, also
    /// when the caller stops waiting on the work it guards.
    pub(crate) fn close_on_drop(&self, broken: Broken) -> CloseOnDrop<'_> {
        CloseOnDrop {
            machine: self,
            broken,
        }
    }

    /// Close the channel for a broken rule and answer the port error.
    pub(crate) fn violation(&self, broken: Broken) -> SandboxError {
        let why = format!("the guest broke the protocol: {broken}");
        self.channel.close(&why);
        self.unavailable(&why)
    }

    /// Run `f` on the egress proxy; refused once the machine is stopping.
    pub(crate) fn with_egress<T>(
        &self,
        f: impl FnOnce(&EgressProxy) -> T,
    ) -> Result<T, SandboxError> {
        match lock(&self.egress).as_ref() {
            Some(proxy) => Ok(f(proxy)),
            None => Err(self.unavailable("it is stopping")),
        }
    }

    /// Check a new ack of `op` and record it.
    pub(crate) fn record_ack(
        &self,
        op: &str,
        state: OperationState,
        existing: bool,
    ) -> Result<(), Broken> {
        let mut book = lock(&self.book);
        let known = book.ops.get(op).copied();
        let broken = match (existing, known) {
            _ if state == OperationState::Unknown => Some("an ack without a state"),
            (false, Some(_)) => Some("the guest started an acknowledged operation again"),
            (false, None)
                if !matches!(state, OperationState::Accepted | OperationState::Running) =>
            {
                Some("a new operation acked a final state")
            }
            (true, Some(Some(settled))) if settled != state => Some("a receipt changed"),
            _ => None,
        };
        if let Some(broken) = broken {
            return Err(broken);
        }
        let settled = state.is_terminal().then_some(state);
        let entry = book.ops.entry(op.to_owned()).or_insert(None);
        *entry = entry.or(settled);
        Ok(())
    }

    /// Check a reported state of `op` against the book and record it.
    pub(crate) fn record_state(&self, op: &str, state: OperationState) -> Result<(), Broken> {
        let mut book = lock(&self.book);
        match (book.ops.get(op).copied(), state) {
            (Some(_), OperationState::Unknown) => Err("the guest forgot an acknowledged operation"),
            (None, OperationState::Unknown) => Ok(()),
            (Some(Some(settled)), state) if settled != state => Err("a receipt changed"),
            (_, state) => {
                let settled = state.is_terminal().then_some(state);
                let entry = book.ops.entry(op.to_owned()).or_insert(None);
                *entry = entry.or(settled);
                Ok(())
            }
        }
    }

    /// Stamp `op` on denials from now on; answers the previous mark.
    pub(crate) fn mark_current(&self, op: &str) -> Option<String> {
        let mut book = lock(&self.book);
        let previous = book.current.replace(op.to_owned());
        self.set_egress_operation(Some(op.to_owned()));
        previous
    }

    /// Undo [`Self::mark_current`] for `op` when nothing started.
    pub(crate) fn restore_current(&self, op: &str, previous: Option<String>) {
        let mut book = lock(&self.book);
        if book.current.as_deref() == Some(op) {
            book.current = previous.clone();
            self.set_egress_operation(previous);
        }
    }

    /// Clear the mark once `op` settled.
    pub(crate) fn settled(&self, op: &str) {
        let mut book = lock(&self.book);
        if book.current.as_deref() == Some(op) {
            book.current = None;
            self.set_egress_operation(None);
        }
    }

    fn set_egress_operation(&self, op: Option<String>) {
        if let Some(proxy) = lock(&self.egress).as_ref() {
            proxy.set_current_operation(op);
        }
    }

    /// Ask the guest to end everything and wait (bounded) for `bye`, then
    /// close the channel and the egress proxy and remove the control
    /// socket. The container itself is the launcher's to stop.
    pub(crate) async fn shutdown(&self) {
        if self.unreachable().is_none() {
            let _ = self.shutdown_guest().await;
        }
        self.channel.close("the machine was stopped");
        let proxy = lock(&self.egress).take();
        if let Some(proxy) = proxy {
            // Dropping the proxy (on timeout) also cancels every tunnel.
            let _ = tokio::time::timeout(EGRESS_SHUTDOWN, proxy.shutdown()).await;
        }
        let _ = std::fs::remove_file(&self.control_path);
    }
}
