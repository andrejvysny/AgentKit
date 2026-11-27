//! macOS microVM launcher of the Track B sandbox backend (ADR 0040).
//!
//! [`VmLauncher`] is an in-process service that answers the existing
//! launcher protocol (`agentkit-sandbox-proto::launcher`: `ensure`, `stop`,
//! `inspect`, `usage`, `reset_home`, `destroy`) on a unix socket and runs
//! one `agentkit-sandbox-vmhelper` process per machine instead of a
//! container. The desktop composes the unchanged `ContainerBackend` with
//! `launcher_socket` = [`VmLauncher::listen_socket`] and `socket_dir` =
//! [`VmLauncher::socket_root`]; the guest channel, the egress proxy, the
//! generation rules, the registry and the mapping are the server's code.
//!
//! - Generations: a generation never runs twice. `ensure` refuses one at
//!   or below a generation proven stopped or below one it started; `stop`
//!   answers `proven` only when the helper of the covered generation has
//!   exited and been reaped. The records persist in `<data_root>/state`.
//! - Startup: the kernel and root disk are verified by SHA-256; every
//!   helper recorded by an earlier run that is still ours (pid and process
//!   start time) is SIGKILLed and proven gone, and its generation recorded
//!   stopped.
//! - Disks: `<data_root>/workspaces/<ws>/{workspace,home}.img`, raw and
//!   sparse, formatted by the guest; usage is allocated bytes.
//! - Access: the socket is 0600 in the 0700 socket root and only this
//!   user's processes are served.
//! - Channel token: a fresh random one per started generation, passed to
//!   the guest on the kernel command line over the helper's stdin and
//!   answered in `ensured`; kept only in memory.
//!
//! [`helper`] documents the helper's argv, stdin and stdout contract.

mod config;
mod dirs;
mod disks;
mod error;
pub mod helper;
mod images;
mod locks;
mod machine;
mod naming;
mod procinfo;
mod recovery;
mod server;
mod service;
mod state;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinHandle;

pub use config::{ImageFile, VmLauncherConfig};
pub use error::{VmConfigError, VmShutdownError, VmStartupError};

use crate::dirs::{SocketDirs, private_child};
use crate::images::Images;
use crate::service::{Service, log};
use crate::state::StateStore;

/// A running microVM launcher. Dropping it stops serving; each helper then
/// sees EOF on its stdin and stops its VM on its own, and the next start
/// recovers any that did not.
pub struct VmLauncher {
    service: Arc<Service>,
    server: Option<JoinHandle<()>>,
    listen_socket: PathBuf,
    socket_root: PathBuf,
    image_digest: String,
}

impl std::fmt::Debug for VmLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmLauncher")
            .field("listen_socket", &self.listen_socket)
            .field("image_digest", &self.image_digest)
            .finish_non_exhaustive()
    }
}

impl VmLauncher {
    /// Validate `config`, prepare the data and socket roots, verify the
    /// images, lock and read the state, recover helpers of an earlier run,
    /// bind the socket and serve on the current tokio runtime.
    pub async fn start(config: VmLauncherConfig) -> Result<Self, VmStartupError> {
        config.validate()?;
        let own_uid = agentkit_sys::effective_uid()
            .map_err(|error| VmStartupError::Directory(error.to_string()))?;
        prepare_dirs(&config, own_uid)?;
        let socket_lock = lock_socket_root(&config.socket_root)?;
        check_helper(&config.helper)?;
        let (kernel, root) = (config.kernel.clone(), config.root_disk.clone());
        let images = tokio::task::spawn_blocking(move || Images::verify(&kernel, &root))
            .await
            .map_err(|error| VmStartupError::Image(error.to_string()))?
            .map_err(VmStartupError::Image)?;
        let state = StateStore::open(&config.data_root.join("state"))
            .map_err(|error| VmStartupError::State(error.to_string()))?;
        let sockets = SocketDirs::new(config.socket_root.clone(), own_uid);
        let recovered = recovery::recover(&state, &sockets)
            .await
            .map_err(VmStartupError::Recovery)?;
        let listen_socket = config.listen_socket();
        let listener = server::bind(&listen_socket).map_err(VmStartupError::Listen)?;
        let config = Arc::new(config);
        let service = Arc::new(Service::new(
            Arc::clone(&config),
            images,
            state,
            socket_lock,
            own_uid,
        ));
        let server = tokio::spawn(server::serve(
            listener,
            Arc::clone(&service),
            own_uid,
            config.max_connections,
        ));
        log(&format!(
            "started; {recovered} helpers of an earlier run recovered"
        ));
        Ok(Self {
            service,
            server: Some(server),
            listen_socket,
            socket_root: config.socket_root.clone(),
            image_digest: config.image_digest(),
        })
    }

    /// The socket the adapter's `launcher_socket` names.
    pub fn listen_socket(&self) -> &Path {
        &self.listen_socket
    }

    /// The directory the adapter's `socket_dir` names.
    pub fn socket_root(&self) -> &Path {
        &self.socket_root
    }

    /// The image digest machine specs must name (`sha256:<root disk>`).
    pub fn image_digest(&self) -> &str {
        &self.image_digest
    }

    /// Stop every machine (the longest grace, then SIGKILL), record the
    /// proven ones stopped, then stop serving.
    pub async fn shutdown(self) -> Result<(), VmShutdownError> {
        let outcome = self.service.stop_all().await;
        self.close().await;
        outcome
    }

    /// Stop serving and release the state directory without stopping
    /// machines: each helper sees EOF on its stdin and stops on its own,
    /// and the next start recovers any that are still running.
    pub async fn close(mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
    }
}

impl Drop for VmLauncher {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

fn prepare_dirs(config: &VmLauncherConfig, own_uid: u32) -> Result<(), VmStartupError> {
    let refused = |error: std::io::Error| VmStartupError::Directory(error.to_string());
    dirs::prepare_root(&config.data_root, own_uid).map_err(refused)?;
    dirs::prepare_root(&config.socket_root, own_uid).map_err(refused)?;
    private_child(&config.data_root.join("state"), own_uid).map_err(refused)
}

/// The socket root's lock, held as long as the service lives.
fn lock_socket_root(root: &Path) -> Result<std::fs::File, VmStartupError> {
    let anchor = agentkit_sys::open_dir_nofollow(root)
        .map_err(|error| VmStartupError::Directory(error.to_string()))?;
    state::lock_dir(&anchor).map_err(|error| {
        VmStartupError::Directory(format!("socket root {}: {error}", root.display()))
    })
}

fn check_helper(path: &Path) -> Result<(), VmStartupError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(VmStartupError::Helper(format!(
            "{} is not a file",
            path.display()
        ))),
        Err(error) => Err(VmStartupError::Helper(format!(
            "{}: {error}",
            path.display()
        ))),
    }
}
