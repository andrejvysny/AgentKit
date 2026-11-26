//! The operation table of this generation: op id → argument hash, state
//! and what the operation needs while it runs. In memory only; a new
//! generation starts empty, so an id from another generation is unknown.
//!
//! Rules (INV-2, INV-7 at the guest): the same id with the same argument
//! hash answers the recorded state and starts nothing; another hash is a
//! conflict; a recorded terminal state never changes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use agentkit_sandbox_proto::guest::OperationState;
use tokio::sync::Notify;

use crate::output::OpLog;

/// True for the states an operation ends in.
pub(crate) fn is_terminal(state: OperationState) -> bool {
    matches!(
        state,
        OperationState::Succeeded { .. }
            | OperationState::Failed { .. }
            | OperationState::TimedOut
            | OperationState::Cancelled
            | OperationState::OutputLimit
    )
}

/// `Failed` with an exit status.
pub(crate) fn failed(exit: i32) -> OperationState {
    OperationState::Failed {
        exit: Some(exit),
        signal: None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a running command shares with the requests that observe it.
#[derive(Default)]
pub(crate) struct ExecOp {
    /// The output log, once created.
    pub(crate) log: OnceLock<Arc<OpLog>>,
    /// `cancel` asks the supervising task to end the command.
    pub(crate) cancel: Notify,
}

/// The kind of a recorded operation.
pub(crate) enum OpKind {
    Exec(ExecOp),
    Write,
}

/// One recorded operation.
pub(crate) struct OpEntry {
    args_sha256: String,
    state: Mutex<OperationState>,
    kind: OpKind,
}

impl OpEntry {
    /// The state now.
    pub(crate) fn state(&self) -> OperationState {
        *lock(&self.state)
    }

    /// `Accepted` → `Running`; any other state is left alone.
    pub(crate) fn mark_running(&self) {
        let mut state = lock(&self.state);
        if *state == OperationState::Accepted {
            *state = OperationState::Running;
        }
    }

    /// Record the terminal state `end`, unless one is recorded already:
    /// the first terminal state is the receipt.
    pub(crate) fn finish(&self, end: OperationState) {
        let mut state = lock(&self.state);
        if !is_terminal(*state) {
            *state = end;
        }
    }

    /// The command side of an exec.
    pub(crate) fn exec(&self) -> Option<&ExecOp> {
        match &self.kind {
            OpKind::Exec(exec) => Some(exec),
            OpKind::Write => None,
        }
    }
}

/// The outcome of [`OpTable::admit`].
pub(crate) enum Admission {
    /// The id is new: recorded as `Accepted`; the caller starts it.
    New(Arc<OpEntry>),
    /// The id is known with the same hash; its state now.
    Existing(OperationState),
    /// The id is known with another hash.
    Conflict,
}

/// Every operation of this generation.
#[derive(Default)]
pub(crate) struct OpTable {
    ops: Mutex<HashMap<String, Arc<OpEntry>>>,
}

impl OpTable {
    /// Record `op` as new, or answer what is recorded for it.
    pub(crate) fn admit(&self, op: &str, args_sha256: &str, kind: OpKind) -> Admission {
        let mut ops = lock(&self.ops);
        if let Some(existing) = ops.get(op) {
            if existing.args_sha256 != args_sha256 {
                return Admission::Conflict;
            }
            return Admission::Existing(existing.state());
        }
        let entry = Arc::new(OpEntry {
            args_sha256: args_sha256.to_owned(),
            state: Mutex::new(OperationState::Accepted),
            kind,
        });
        ops.insert(op.to_owned(), entry.clone());
        Admission::New(entry)
    }

    /// The operation `op`, when this generation knows it.
    pub(crate) fn get(&self, op: &str) -> Option<Arc<OpEntry>> {
        lock(&self.ops).get(op).cloned()
    }

    /// True while some command has no terminal state yet.
    pub(crate) fn exec_open(&self) -> bool {
        lock(&self.ops)
            .values()
            .any(|entry| entry.exec().is_some() && !is_terminal(entry.state()))
    }
}

#[cfg(test)]
mod tests;
