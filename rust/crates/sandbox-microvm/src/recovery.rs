//! Startup recovery: every helper the state still records was started by
//! an earlier run. One that is still ours (same pid AND same start time)
//! is SIGKILLed, which ends its VM, and waited for until it is gone; one
//! whose pid is free or names another process is gone already. Each
//! recorded generation is then proven stopped and recorded so, and its
//! machine directory removed. A helper that cannot be proven gone stops
//! the launcher from starting (fail closed).
//!
//! After a real restart the old helpers are not this process's children
//! (their parent died; launchd reaps them). When they are (a launcher
//! restarted inside one process), a killed helper is reaped here, but only
//! after its start time proved it is the recorded one.

use std::thread::sleep;
use std::time::{Duration, Instant};

use agentkit_sys::{Signal, reap_child, signal_process};

use crate::dirs::SocketDirs;
use crate::procinfo::{Lookup, lookup};
use crate::service::log;
use crate::state::{HelperRecord, StateStore};

/// How long a killed helper gets to disappear.
const GONE_WITHIN: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// Kill and prove gone every recorded helper; answers how many were
/// recorded.
pub(crate) async fn recover(state: &StateStore, sockets: &SocketDirs) -> Result<usize, String> {
    let helpers = state.helpers().await;
    for (ws, helper) in &helpers {
        let helper = *helper;
        tokio::task::spawn_blocking(move || reclaim(helper))
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
        let found = lookup(helper.pid).map_err(|error| error.to_string())?;
        match found {
            Lookup::Gone => return Ok(()),
            Lookup::Found { started, .. } if started != helper.started => return Ok(()),
            Lookup::Found { zombie: true, .. } => {
                // Dead; collect it when it is this process's own child.
                let _ = reap_child(helper.pid);
                return Ok(());
            }
            Lookup::Found { .. } if !killed => {
                signal_process(helper.pid, Signal::Kill).map_err(|error| error.to_string())?;
                log(&format!("killed leftover helper pid {}", helper.pid));
                killed = true;
            }
            Lookup::Found { .. } => {
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
