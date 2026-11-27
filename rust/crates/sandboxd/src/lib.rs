//! Guest supervisor of the Track B sandbox backend (ADR 0039). The binary
//! runs as PID 1 of a sandbox container and serves the host over the
//! framed protocol of `agentkit-sandbox-proto`: it dials the host's control
//! socket, checks `hello` and answers it with the launcher's channel token
//! (never logged), then runs commands and file operations for the
//! host until `shutdown` or until the channel is lost, when it ends every
//! process, syncs and exits.
//!
//! - Commands: `bash -c` in a new session, clean
//!   environment from the image's environment file, output through a pipe
//!   into `/workspace/.sandbox/logs/<op>.log` bounded by the output limit,
//!   deadline, cancel, and one receipt per operation id.
//! - Files: atomic, digest-checked writes; bounded reads and listings; a
//!   usage count that says when part of a tree could not be measured.
//!   Every path below `/workspace` and `/home/agent` is resolved one
//!   component at a time without following links.
//! - Processes: what ends with an operation depends on the
//!   [`ProcessMode`]; as PID 1 every process but the supervisor ends.
//! - Egress: a TCP shim on `127.0.0.1:3128` piping to the host's egress
//!   socket.
//! - Transport: in a container both links are unix sockets; in a microVM
//!   guest (Linux, ADR 0040 §4) the supervisor listens on a vsock port
//!   for its one control connection and the shim pipes to the host over
//!   vsock ([`Transport`]). The session over either is the same.
//!
//! Logs go to stderr and name lifecycle events only, never a command or
//! its output.
//!
//! The core is portable Unix and tested on macOS; `prctl`, `kill(-1)` as
//! PID 1 and the `/proc` descendant sweep are Linux-only and verified in a
//! container of the image.

mod config;
mod conn;
mod envfile;
mod error;
mod exec;
mod files;
mod listing;
mod optable;
mod output;
mod paths;
mod reaper;
mod server;
mod shim;
// Only a vsock guest (Linux) accepts its control channel; the tests drive
// the same accept path over a unix listener everywhere.
#[cfg(any(target_os = "linux", test))]
mod transport;
mod usage;
mod write;

use std::io;

pub use config::{Config, ProcessMode, Transport};
pub use error::{ConfigError, HandshakeError, StartupError};
pub use server::{Exit, run, run_with, serve};

/// Make the process non-dumpable and a child subreaper (Linux), before
/// anything else runs. Embedded supervisors share their process and
/// change nothing; on platforms without `prctl` this is a no-op.
pub fn harden_process(mode: ProcessMode) -> io::Result<()> {
    if mode == ProcessMode::Embedded {
        return Ok(());
    }
    required(agentkit_sys::set_non_dumpable())?;
    required(agentkit_sys::set_child_subreaper())
}

fn required(outcome: io::Result<()>) -> io::Result<()> {
    match outcome {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            eprintln!("agentkit-sandboxd: process hardening is unsupported on this platform");
            Ok(())
        }
        other => other,
    }
}
