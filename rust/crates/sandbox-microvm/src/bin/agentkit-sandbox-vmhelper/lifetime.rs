//! What ends a VM, gathered on one channel: EOF on stdin (the launcher is
//! gone or wants the machine stopped), SIGTERM, the parent's death (polled,
//! since macOS has no parent-death signal), the control bridge closing,
//! and the VM stopping by itself. A watchdog bounds the stop.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use agentkit_sandbox_microvm::helper::HelperEvent;

use crate::events::emit;

/// How often the parent's pid is checked.
const PARENT_POLL: Duration = Duration::from_millis(500);

/// Something that ends the VM.
pub(crate) enum Event {
    /// Stop the VM, for this reason.
    Stop(String),
    /// The guest powered the VM off.
    GuestStopped,
    /// The VM stopped on an error.
    VmFailed(String),
}

/// Block SIGTERM in the calling thread. Call first in `main`, before any
/// thread or dispatch queue exists: every thread started later inherits
/// the mask, so the signal stays pending for [`watch_sigterm`] instead of
/// killing the process.
pub(crate) fn block_sigterm() -> std::io::Result<()> {
    // SAFETY: `set` is a local signal set initialised by `sigemptyset`
    // before use; `pthread_sigmask` reads it and writes nothing else.
    let rc = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut())
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc))
    }
}

/// Wait for SIGTERM on a thread of its own.
pub(crate) fn watch_sigterm(events: Sender<Event>) {
    thread::spawn(move || {
        let mut signal: libc::c_int = 0;
        // SAFETY: `set` is initialised by `sigemptyset` before use and
        // `signal` is a writable local; SIGTERM is blocked in every thread
        // (see `block_sigterm`), so `sigwait` receives it.
        let rc = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigwait(&set, &mut signal)
        };
        let reason = if rc == 0 {
            "SIGTERM".to_owned()
        } else {
            format!("sigwait failed ({rc})")
        };
        let _ = events.send(Event::Stop(reason));
    });
}

/// Read the rest of stdin until EOF, then ask for a stop.
pub(crate) fn watch_stdin(mut stdin: impl Read + Send + 'static, events: Sender<Event>) {
    thread::spawn(move || {
        let _ = std::io::copy(&mut stdin, &mut std::io::sink());
        let _ = events.send(Event::Stop("stdin closed".to_owned()));
    });
}

/// Ask for a stop once the helper's parent is gone (reparented to
/// launchd, pid 1).
pub(crate) fn watch_parent(events: Sender<Event>) {
    thread::spawn(move || {
        loop {
            if agentkit_sys::own_parent_pid().is_ok_and(|ppid| ppid == 1) {
                let _ = events.send(Event::Stop("the parent process is gone".to_owned()));
                return;
            }
            thread::sleep(PARENT_POLL);
        }
    });
}

/// Exit with status 3 if the process still runs after `within`.
pub(crate) fn arm_watchdog(within: Duration) {
    thread::spawn(move || {
        thread::sleep(within);
        emit(&HelperEvent::Stopped {
            reason: "the stop did not finish in time".to_owned(),
        });
        std::process::exit(3);
    });
}
