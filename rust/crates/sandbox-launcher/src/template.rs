//! The fixed container template (ADR 0039, "Launcher"), as the Engine API
//! create body. Requests choose only the workspace, the generation and a
//! configured tier; everything else is fixed here or in the configuration.
//!
//! Runtime facts it follows (`wp-2.1-runtime-probes.md`, "Consequences"):
//! the image by `<repository>@<index digest>`; `PidsLimit` always positive
//! (Podman maps 0 to 1); `MemorySwap` equal to `Memory`; `Init` false;
//! explicit `noexec` tmpfs for `/tmp` and `/var/tmp`; volumes with the
//! default copy-up, never `NoCopy`; no restart policy and no auto-remove,
//! so `stop` can prove absence itself.

use agentkit_sandbox_proto::launcher::Tier;
use serde_json::{Value, json};

use crate::config::Config;
use crate::naming::{Names, WorkspaceId, container_labels};
use crate::token::TOKEN_ENV;

/// The supervisor, PID 1 of every sandbox container.
pub(crate) const SUPERVISOR: &str = "/usr/local/bin/agentkit-sandboxd";
/// Where the per-machine socket directory appears in the container.
pub(crate) const SOCKET_MOUNT: &str = "/run/agentkit";
/// The workload's user; the image's `/workspace` and `/home/agent` belong
/// to it, which the volumes' copy-up relies on.
const USER: &str = "1000:1000";

/// What one container is created for. No `Debug`: it holds the channel
/// token, which is never printed.
#[derive(Clone, Copy)]
pub(crate) struct Machine<'a> {
    pub ws: &'a WorkspaceId,
    pub generation: u64,
    pub tier: &'a Tier,
    /// Host path of the machine directory (`<host>/<machine_dir_name>`).
    pub socket_dir: &'a str,
    /// The container's channel token, placed in its environment.
    pub channel_token: &'a str,
}

/// The create body for `machine`.
pub(crate) fn container_body(config: &Config, machine: Machine<'_>) -> Value {
    json!({
        "Image": config.image_reference,
        "Entrypoint": [SUPERVISOR],
        "Cmd": [],
        "Env": environment(config, machine),
        "User": USER,
        "WorkingDir": "/workspace",
        "Labels": container_labels(machine.ws, machine.generation, &config.image_digest),
        "HostConfig": host_config(config, machine),
    })
}

fn environment(config: &Config, machine: Machine<'_>) -> Vec<String> {
    vec![
        format!("AGENTKIT_GENERATION={}", machine.generation),
        format!("AGENTKIT_IMAGE_DIGEST={}", config.image_digest),
        format!("{TOKEN_ENV}={}", machine.channel_token),
        format!("AGENTKIT_CONTROL_SOCKET={SOCKET_MOUNT}/control.sock"),
        format!("AGENTKIT_EGRESS_SOCKET={SOCKET_MOUNT}/egress.sock"),
    ]
}

fn host_config(config: &Config, machine: Machine<'_>) -> Value {
    let tier = machine.tier;
    let names = Names::new(&config.name_prefix);
    json!({
        "NetworkMode": "none",
        "CapDrop": ["ALL"],
        "SecurityOpt": ["no-new-privileges"],
        "Privileged": false,
        "ReadonlyRootfs": true,
        "Tmpfs": {
            "/tmp": tmpfs_options(config.tmp_size_bytes),
            "/var/tmp": tmpfs_options(config.var_tmp_size_bytes),
        },
        "PidsLimit": tier.pids,
        "Memory": tier.memory_bytes,
        "MemorySwap": tier.memory_bytes,
        "NanoCpus": u64::from(tier.cpus_milli) * 1_000_000,
        "Init": false,
        "AutoRemove": false,
        "RestartPolicy": { "Name": "no" },
        "Mounts": [
            {
                "Type": "volume",
                "Source": names.workspace_volume(machine.ws),
                "Target": "/workspace",
            },
            {
                "Type": "volume",
                "Source": names.home_volume(machine.ws),
                "Target": "/home/agent",
            },
            {
                "Type": "bind",
                "Source": machine.socket_dir,
                "Target": SOCKET_MOUNT,
                "ReadOnly": true,
            },
        ],
    })
}

fn tmpfs_options(size_bytes: u64) -> String {
    format!("rw,nosuid,nodev,noexec,size={size_bytes}")
}

#[cfg(test)]
mod tests;
