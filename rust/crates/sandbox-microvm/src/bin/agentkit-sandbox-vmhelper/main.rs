//! `agentkit-sandbox-vmhelper`: one Virtualization.framework microVM per
//! process (ADR 0040, section 3), started by the microVM launcher.
//!
//! Contract (see `agentkit_sandbox_microvm::helper`): no arguments; one
//! JSON configuration line on stdin, whose write end the launcher keeps
//! open; JSON event lines on stdout (`started`, `connected`, `stopped`);
//! diagnostics on stderr. EOF on stdin, SIGTERM, the parent's death, the
//! control channel closing or the guest powering off stop the VM: the
//! control bridge is closed, the guest is asked to stop, after 3 s the VM
//! is forced off, then the helper exits. The VM never outlives the helper
//! (it lives in Apple's VM service process, which dies with its client).
//!
//! Exit status: 0 when the VM reached the stopped state, 1 when it ended
//! in another state, 2 when the configuration or the start was refused,
//! 3 when the stop outran its watchdog. macOS only; elsewhere it exits 2.
//!
//! Signed with exactly one entitlement, `com.apple.security.virtualization`
//! (`entitlements/vmhelper.plist`, `scripts/sign-helper.sh`).

// The bridges are plain descriptors, so their tests run on every Unix.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod bridge;
#[cfg(target_os = "macos")]
mod config;
#[cfg(target_os = "macos")]
mod delegate;
#[cfg(target_os = "macos")]
mod events;
#[cfg(target_os = "macos")]
mod lifetime;
#[cfg(target_os = "macos")]
mod run;
#[cfg(target_os = "macos")]
mod vm;

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(run::main());
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("agentkit-sandbox-vmhelper: Virtualization.framework needs macOS");
    std::process::exit(2);
}
