//! The helper's two outputs: event lines on stdout for the launcher, and
//! diagnostics on stderr (the launcher writes them to `helper.log`).
//! Neither ever carries the configuration's command line.

use std::io::Write;

use agentkit_sandbox_microvm::helper::HelperEvent;

/// Write one event line and flush it. A launcher that is gone is not an
/// error here; its EOF on stdin stops the VM.
pub(crate) fn emit(event: &HelperEvent) {
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

/// One diagnostic line.
pub(crate) fn diag(message: &str) {
    eprintln!("agentkit-sandbox-vmhelper: {message}");
}
