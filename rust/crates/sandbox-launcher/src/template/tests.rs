use std::path::Path;

use serde_json::json;

use super::*;
use crate::fake::{DIGEST, test_config};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn body() -> Value {
    let config = test_config(&std::fs::canonicalize(std::env::temp_dir()).unwrap());
    let config = Config {
        socket_host_dir: Path::new("/run/aks").into(),
        ..config
    };
    let ws = WorkspaceId::parse("ws-1").unwrap();
    let tier = Tier {
        cpus_milli: 1500,
        memory_bytes: 1 << 30,
        pids: 256,
    };
    let machine = Machine {
        ws: &ws,
        generation: 7,
        tier: &tier,
        socket_dir: "/run/aks/m0123456789abcdef0123456",
        channel_token: TOKEN,
    };
    container_body(&config, machine)
}

/// The whole create body, field by field.
#[test]
fn create_body_snapshot() {
    let expected = json!({
        "Image": format!("localhost/agentkit-sandbox@{DIGEST}"),
        "Entrypoint": ["/usr/local/bin/agentkit-sandboxd"],
        "Cmd": [],
        "Env": [
            "AGENTKIT_GENERATION=7",
            format!("AGENTKIT_IMAGE_DIGEST={DIGEST}"),
            format!("AGENTKIT_CHANNEL_TOKEN={TOKEN}"),
            "AGENTKIT_CONTROL_SOCKET=/run/agentkit/control.sock",
            "AGENTKIT_EGRESS_SOCKET=/run/agentkit/egress.sock",
        ],
        "User": "1000:1000",
        "WorkingDir": "/workspace",
        "Labels": {
            "agentkit.sandbox.generation": "7",
            "agentkit.sandbox.image": DIGEST,
            "agentkit.sandbox.managed": "1",
            "agentkit.sandbox.workspace": "ws-1",
        },
        "HostConfig": {
            "NetworkMode": "none",
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges"],
            "Privileged": false,
            "ReadonlyRootfs": true,
            "Tmpfs": {
                "/tmp": "rw,nosuid,nodev,noexec,size=268435456",
                "/var/tmp": "rw,nosuid,nodev,noexec,size=67108864",
            },
            "PidsLimit": 256,
            "Memory": 1_073_741_824u64,
            "MemorySwap": 1_073_741_824u64,
            "NanoCpus": 1_500_000_000u64,
            "Init": false,
            "AutoRemove": false,
            "RestartPolicy": { "Name": "no" },
            "Mounts": [
                { "Type": "volume", "Source": "aks-ws-ws-1", "Target": "/workspace" },
                { "Type": "volume", "Source": "aks-home-ws-1", "Target": "/home/agent" },
                {
                    "Type": "bind",
                    "Source": "/run/aks/m0123456789abcdef0123456",
                    "Target": "/run/agentkit",
                    "ReadOnly": true,
                },
            ],
        },
    });
    assert_eq!(body(), expected);
}

/// The properties the threat model rests on, checked one by one so a
/// snapshot update cannot drop one silently.
#[test]
fn create_body_keeps_the_security_properties() {
    let body = body();
    let host = &body["HostConfig"];
    assert_eq!(host["NetworkMode"], "none");
    assert_eq!(host["CapDrop"], json!(["ALL"]));
    assert_eq!(host["ReadonlyRootfs"], true);
    assert_eq!(host["Privileged"], false);
    assert_eq!(host["Init"], false);
    assert_eq!(host["AutoRemove"], false);
    assert!(host["PidsLimit"].as_u64().unwrap() > 0);
    assert_eq!(host["MemorySwap"], host["Memory"]);
    assert!(host["Memory"].as_u64().unwrap() > 0);
    assert!(host["NanoCpus"].as_u64().unwrap() > 0);
    for tmpfs in ["/tmp", "/var/tmp"] {
        let options = host["Tmpfs"][tmpfs].as_str().unwrap();
        assert!(options.contains("noexec") && options.contains("size="));
    }
    let mounts = host["Mounts"].as_array().unwrap();
    let bind = mounts.iter().find(|m| m["Type"] == "bind").unwrap();
    assert_eq!(bind["ReadOnly"], true);
    assert_eq!(bind["Target"], "/run/agentkit");
    let text = body.to_string();
    for absent in [
        "NoCopy",
        "VolumeOptions",
        "Binds",
        "Devices",
        "CapAdd",
        "PortBindings",
        "NetworkingConfig",
        "Volumes",
    ] {
        assert!(!text.contains(absent), "{absent} in {text}");
    }
}
