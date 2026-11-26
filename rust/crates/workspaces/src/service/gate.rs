//! Admission and the desktop quit (spec "Desktop quit", F-13): once
//! `quiesce` closes admission every new call answers `sandbox_unavailable`,
//! the running operation is cancelled and given the cancel grace for its
//! receipt (settled as interrupted with it, else uncertain and its machine
//! stopped), and then every machine this instance runs is stopped.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio_util::sync::CancellationToken;

use crate::error::SandboxError;

use super::WorkspaceService;
use super::types::{QuiesceReport, StopOutcome};

/// Whether calls are admitted, and how many admitted ones still run.
#[derive(Default)]
pub(super) struct Gate {
    closed: AtomicBool,
    quit: CancellationToken,
    in_flight: AtomicUsize,
}

/// One admitted call; dropping it (on any exit) counts it out.
pub(super) struct Admitted<'a>(&'a Gate);

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn shutting_down() -> SandboxError {
    SandboxError::Unavailable {
        reason: "the sandbox is shutting down".into(),
    }
}

impl Gate {
    /// Count a new call in, or refuse it once admission is closed.
    pub(super) fn admit(&self) -> Result<Admitted<'_>, SandboxError> {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let admitted = Admitted(self);
        if self.closed.load(Ordering::SeqCst) {
            return Err(shutting_down());
        }
        Ok(admitted)
    }

    /// Refuse when admission is closed (checked again before a row is
    /// written, so a call admitted just before the quit dispatches nothing).
    pub(super) fn check_open(&self) -> Result<(), SandboxError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(shutting_down());
        }
        Ok(())
    }

    /// True once the service quits: running operations are cancelled.
    pub(super) fn quitting(&self) -> bool {
        self.quit.is_cancelled()
    }

    pub(super) fn quit_token(&self) -> &CancellationToken {
        &self.quit
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.quit.cancel();
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

impl WorkspaceService {
    /// The desktop quit (F-13). Closes admission, cancels the running
    /// operations and waits until every admitted call has returned (each
    /// waits up to the cancel grace for its receipt), then stops every
    /// machine this instance runs. Idempotent.
    pub async fn quiesce(&self) -> Result<QuiesceReport, SandboxError> {
        self.gate.close();
        while self.gate.in_flight() > 0 {
            self.pause.pause(self.config.poll_interval_ms).await;
        }
        let mut report = QuiesceReport::default();
        for ws in self.machines.least_recently_used(None) {
            match self.stop_unleased(&ws, "quit").await? {
                Some(StopOutcome::Stopped { .. }) => report.stopped.push(ws),
                Some(StopOutcome::Fenced { .. }) => report.fenced.push(ws),
                Some(StopOutcome::NotStarted) | None => {}
            }
        }
        Ok(report)
    }
}
