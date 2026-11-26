//! Test support: a fake launcher, a scripted guest, the real supervisor
//! in-process, fake egress ports, and helpers for a real launcher.

#![allow(dead_code)]

pub mod embedded;
pub mod guest;
pub mod launcher;
pub mod real;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_egress::{Connector, EgressPorts, Resolver};
use agentkit_sandbox_container::{ContainerBackend, ContainerBackendConfig};
use agentkit_testkit::conformance::sandbox_backend::SandboxDriver;
use agentkit_workspaces::{Generation, MachineSpec, NetworkPolicy, ResourceLimits, WorkspaceId};
use async_trait::async_trait;
use tokio::net::TcpStream;

use launcher::{FakeLauncher, Starter};

pub const DIGEST: &str = "sha256:conformance-image";

/// Lets real time pass.
pub struct Sleeper;

#[async_trait]
impl SandboxDriver for Sleeper {
    async fn elapse(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// A config for `launcher` with short waits.
pub fn config(launcher: &FakeLauncher) -> ContainerBackendConfig {
    ContainerBackendConfig {
        handshake_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        launcher_timeout: Duration::from_secs(10),
        stop_grace_ms: 2_000,
        ..ContainerBackendConfig::new(launcher.socket.clone(), launcher.socket_dir.clone())
    }
}

/// A launcher under `root` with `starter`, and a backend over it.
pub fn backend_with(
    root: &Path,
    starter: Starter,
    tune: impl FnOnce(&mut ContainerBackendConfig),
    ports: EgressPorts,
) -> (FakeLauncher, ContainerBackend) {
    let data = root.join("data");
    let launcher = FakeLauncher::start(root, DIGEST, starter, Some(data));
    let mut config = config(&launcher);
    tune(&mut config);
    let backend = ContainerBackend::new(config, ports).unwrap();
    (launcher, backend)
}

pub fn spec(ws: &str, generation: u64) -> MachineSpec {
    MachineSpec {
        workspace_id: WorkspaceId::new(ws),
        generation: Generation(generation),
        image_digest: DIGEST.to_owned(),
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy::default(),
    }
}

/// Resolves every name to one public address.
pub struct FixedResolver(pub IpAddr);

#[async_trait]
impl Resolver for FixedResolver {
    async fn resolve(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
        Ok(vec![self.0])
    }
}

/// Connects every upstream to one local listener and records the dials.
pub struct LocalConnector {
    pub to: SocketAddr,
    pub dialed: Mutex<Vec<SocketAddr>>,
}

#[async_trait]
impl Connector for LocalConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        self.dialed.lock().unwrap().push(addr);
        TcpStream::connect(self.to).await
    }
}

/// Ports whose upstream is a local listener at `to`.
pub fn local_ports(to: SocketAddr) -> EgressPorts {
    let system = EgressPorts::system();
    EgressPorts {
        resolver: Arc::new(FixedResolver("93.184.216.34".parse().unwrap())),
        connector: Arc::new(LocalConnector {
            to,
            dialed: Mutex::new(Vec::new()),
        }),
        runtime: system.runtime,
    }
}
