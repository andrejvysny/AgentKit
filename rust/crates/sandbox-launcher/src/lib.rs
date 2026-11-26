//! Fixed-template container launcher of the Track B sandbox backend
//! (ADR 0039). A separate service that alone holds the container runtime
//! socket and offers only `ensure`, `stop`, `inspect`, `usage`,
//! `reset_home` and `destroy` over its own unix socket
//! (`agentkit-sandbox-proto::launcher`). No request names an image, mount,
//! device, flag or network: the template is fixed, and only the workspace
//! id, the generation and one of the configured tiers come from the caller.
//!
//! - Runtime: the Docker Engine API over its unix socket, which Docker and
//!   Podman's compatible service both answer. No CLI, never a pull.
//! - Generations: `ensure` refuses a generation at or below one proven
//!   stopped or below one it created a container for, and creates at most
//!   one container per generation (a generation never runs twice); both
//!   records persist in the state directory, so the rules survive a
//!   restart. `stop` of a generation stops every container of the
//!   workspace at or below it and answers `proven` only when the runtime
//!   lists none of them. At most `max_containers` containers exist at once.
//! - Socket directories: `<socket_host_dir>/<machine_dir_name(ws, gen)>`,
//!   created without following links and handed to the server's uid,
//!   bind-mounted read-only at `/run/agentkit`. `socket_host_dir` grants
//!   others nothing, and it, the state directory, the listen socket's
//!   directory and every directory above them are checked against
//!   changes by other users (startup, and each start for the socket
//!   directory).
//! - Channel token: each created container gets a fresh random token in
//!   its environment, answered in `ensured`; the guest repeats it in
//!   `hello`. Never logged.
//! - Startup changes nothing in the runtime; it counts the launcher's
//!   containers by label.
//!
//! Deployment: with Docker the launcher runs as root (the socket is
//! root-equivalent); with rootless Podman it runs as the server's user,
//! so there the fixed template holds only against an honest server.
//! The listen socket is mode 0600, or 0660 with `listen_gid` the server's
//! group; the peer's uid must be the server's or the launcher's own.

mod config;
mod dirs;
mod engine;
mod error;
mod listen;
mod locks;
mod naming;
mod runtime;
mod server;
mod service;
mod state;
mod template;
mod token;
mod transport;
mod trust;
mod usage;

#[cfg(test)]
mod fake;

use std::sync::Arc;

pub use config::Config;
pub use error::{ConfigError, StartupError};

use crate::dirs::SocketDirs;
use crate::engine::EngineApi;
use crate::listen::bind_listener;
use crate::server::Peers;
use crate::service::{Service, log};
use crate::state::StateStore;
use crate::trust::TrustRoot;

/// Check the host and state directories, load the state, count what the
/// runtime holds, bind the listen socket and serve until the listener
/// fails.
pub async fn run(config: Config) -> Result<(), StartupError> {
    let config = Arc::new(config);
    let own_uid = agentkit_sys::effective_uid()
        .map_err(|error| StartupError::SocketDir(error.to_string()))?;
    let trust = TrustRoot::system();
    SocketDirs::new(
        config.socket_host_dir.clone(),
        config.server_uid,
        config.server_gid,
        trust.clone(),
    )
    .check_host()
    .map_err(StartupError::SocketDir)?;
    trust
        .open_checked(&config.state_dir, own_uid)
        .map_err(|error| StartupError::State(error.to_string()))?;
    let state = StateStore::open(&config.state_dir)
        .map_err(|error| StartupError::State(error.to_string()))?;
    let runtime = Arc::new(EngineApi::new(config.runtime_socket.clone()));
    let service = Arc::new(Service::new(
        Arc::clone(&config),
        runtime,
        state,
        trust.clone(),
    ));
    match service.reconcile().await {
        Ok(counts) => log(&format!("started: {counts}")),
        Err(error) => log(&format!(
            "started; the runtime is not answering yet: {error}"
        )),
    }
    let listener = bind_listener(&config, &trust, own_uid).map_err(StartupError::Listen)?;
    let peers = Peers {
        server_uid: config.server_uid,
        own_uid,
    };
    server::serve(listener, service, peers, config.max_connections)
        .await
        .map_err(StartupError::Serve)
}
