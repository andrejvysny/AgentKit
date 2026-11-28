//! Startup recovery: every helper the state still records was started by
//! an earlier run. One that is still ours (same pid AND same start time)
//! is SIGKILLed, which ends its VM, and waited for until it is gone; one
//! whose pid is free or names another process is gone already. Once no
//! process has the workspace's disks open either (Apple's VM service
//! process may outlive a killed helper for a moment; polled for up to
//! [`crate::disks::RELEASE_WAIT`]), each recorded generation is proven stopped and
//! recorded so, and its machine directory removed. A helper or a disk
//! holder that cannot be proven gone stops the launcher from starting
//! (fail closed).
//!
//! After a real restart the old helpers are not this process's children
//! (their parent died; launchd reaps them). When they are (a launcher
//! restarted inside one process), a killed helper is reaped here, but only
//! after its start time proved it is the recorded one.

use std::thread::sleep;
use std::time::{Duration, Instant};

use agentkit_sys::{ProcessLookup, Signal, lookup_process, reap_child, signal_process};

use crate::dirs::SocketDirs;
use crate::disks::Disks;
use crate::naming::WorkspaceId;
use crate::service::log;
use crate::state::{HelperRecord, StateStore};

/// How long a killed helper gets to disappear.
const GONE_WITHIN: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// Kill and prove gone every recorded helper and its VM, waiting up to
/// `release` for each workspace's disks; answers how many were recorded.
pub(crate) async fn recover(
    state: &StateStore,
    sockets: &SocketDirs,
    disks: &Disks,
    release: Duration,
) -> Result<usize, String> {
    let helpers = state.helpers().await;
    for (ws, helper) in &helpers {
        let helper = *helper;
        let id = WorkspaceId::parse(ws).map_err(|error| error.to_string())?;
        let disks = disks.clone();
        tokio::task::spawn_blocking(move || {
            reclaim(helper)?;
            disks.wait_released(&id, release)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|why| format!("{ws} generation {}: {why}", helper.generation))?;
        state
            .record_stopped(ws, helper.generation)
            .await
            .map_err(|error| error.to_string())?;
        if let Err(error) = sockets.remove(ws, helper.generation) {
            log(&format!(
                "socket directory of {ws} g{} not removed: {error}",
                helper.generation
            ));
        }
    }
    Ok(helpers.len())
}

/// Make sure the recorded helper is not running. Blocking.
pub(crate) fn reclaim(helper: HelperRecord) -> Result<(), String> {
    let deadline = Instant::now() + GONE_WITHIN;
    let mut killed = false;
    loop {
        let found = lookup_process(helper.pid).map_err(|error| error.to_string())?;
        match found {
            ProcessLookup::Gone => return Ok(()),
            ProcessLookup::Found { started, .. } if started != helper.started => return Ok(()),
            ProcessLookup::Found { zombie: true, .. } => {
                // Dead; collect it when it is this process's own child.
                let _ = reap_child(helper.pid);
                return Ok(());
            }
            ProcessLookup::Found { .. } if !killed => {
                signal_process(helper.pid, Signal::Kill).map_err(|error| error.to_string())?;
                log(&format!("killed leftover helper pid {}", helper.pid));
                killed = true;
            }
            ProcessLookup::Found { .. } => {
                let _ = reap_child(helper.pid);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("helper pid {} survived SIGKILL", helper.pid));
        }
        sleep(POLL);
    }
}

#[cfg(test)]
mod tests;
