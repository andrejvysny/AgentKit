//! The real supervisor as the "container": `agentkit_sandboxd::run` in
//! this process ([`ProcessMode::Embedded`]), with the workspace and home
//! roots of each workspace under a data directory and its egress shim on
//! a free local port that the command environment's proxy variables name.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use agentkit_sandboxd::{Config, ProcessMode};

use super::launcher::{Start, Starter};

/// A port nothing listens on right now.
fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// Starts an embedded supervisor for each machine, its roots under
/// `<data_dir>/<ws>/{workspace,home}`.
pub fn starter(data_dir: PathBuf, digest: &str) -> Starter {
    let digest = digest.to_owned();
    Arc::new(move |start: Start| {
        let root = data_dir.join(&start.workspace_id);
        let workspace = root.join("workspace");
        let home = root.join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let port = free_port();
        let proxy = format!("http://127.0.0.1:{port}");
        let env_file = root.join(format!("environment-g{}", start.generation));
        let env = format!(
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin\nHOME={}\nHTTPS_PROXY={proxy}\nhttps_proxy={proxy}\n",
            home.display()
        );
        std::fs::write(&env_file, env).unwrap();
        let config = Config {
            generation: start.generation,
            image_digest: digest.clone(),
            channel_token: start.channel_token.clone(),
            control_socket: start.dir.join("control.sock"),
            egress_socket: start.dir.join("egress.sock"),
            workspace_root: workspace,
            home_root: home,
            env_file,
            shell: PathBuf::from("/bin/bash"),
            shim_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            mode: ProcessMode::Embedded,
        };
        tokio::spawn(async move {
            let _ = agentkit_sandboxd::run(config).await;
        })
    })
}
