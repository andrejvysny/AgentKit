use serde_json::{Value, json};

use super::*;

pub(crate) const DIGEST: &str =
    "sha256:984736245081e2d2fecd711ead271b0c9ccf273fe096efc12a8dd23e59f9aa0a";

fn minimal() -> Value {
    json!({
        "runtime_socket": "/run/docker.sock",
        "listen_socket": "/run/agentkit-launcher/launcher.sock",
        "image_reference": format!("localhost/agentkit-sandbox@{DIGEST}"),
        "image_digest": DIGEST,
        "socket_host_dir": "/run/agentkit-sandbox",
        "server_uid": 1001,
        "server_gid": 1001,
        "state_dir": "/var/lib/agentkit-launcher",
    })
}

fn load(value: &Value) -> Result<Config, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("launcher.json");
    std::fs::write(&path, value.to_string()).unwrap();
    Config::load(&path)
}

fn with(field: &str, value: Value) -> Value {
    let mut config = minimal();
    config[field] = value;
    config
}

fn refused(value: &Value) -> String {
    match load(value) {
        Err(error) => error.to_string(),
        Ok(config) => panic!("accepted {config:?}"),
    }
}

#[test]
fn minimal_config_takes_the_defaults() {
    let config = load(&minimal()).unwrap();
    assert_eq!(config.listen_mode, 0o600);
    assert_eq!(config.listen_gid, None);
    assert_eq!(config.name_prefix, "aks-");
    assert_eq!(config.tmp_size_bytes, 256 << 20);
    assert_eq!(config.var_tmp_size_bytes, 64 << 20);
    assert_eq!(config.stop_grace_max_ms, 30_000);
    assert_eq!(config.max_connections, 32);
    assert_eq!(config.max_containers, 8);
    let default = Tier {
        cpus_milli: 2000,
        memory_bytes: 2 << 30,
        pids: 512,
    };
    assert_eq!(config.tiers, BTreeMap::from([("default".into(), default)]));
    assert!(config.allows_tier(&default));
    assert!(!config.allows_tier(&Tier {
        pids: 513,
        ..default
    }));
}

#[test]
fn listen_mode_is_octal_and_never_open_to_others() {
    let config = load(&with("listen_mode", json!("0660"))).unwrap();
    assert_eq!(config.listen_mode, 0o660);
    for mode in ["0666", "0700", "0640", "0400"] {
        assert!(refused(&with("listen_mode", json!(mode))).contains("listen_mode"));
    }
    assert!(refused(&with("listen_mode", json!("rw"))).contains("octal"));
}

#[test]
fn image_reference_must_name_the_digest() {
    let other = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let cases = [
        with(
            "image_reference",
            json!(format!("localhost/agentkit-sandbox@{other}")),
        ),
        with("image_reference", json!("localhost/agentkit-sandbox:v0")),
        with(
            "image_reference",
            json!(format!("Localhost/Sandbox@{DIGEST}")),
        ),
        with("image_reference", json!(format!("@{DIGEST}"))),
        with("image_digest", json!("sha256:ABC")),
        with("image_digest", json!(DIGEST.trim_start_matches("sha256:"))),
    ];
    for case in &cases {
        let error = refused(case);
        assert!(error.contains("image_"), "{error}");
    }
}

#[test]
fn tiers_need_positive_bounded_limits() {
    let tier = |cpus: u32, memory: u64, pids: u32| json!({ "small": { "cpus_milli": cpus, "memory_bytes": memory, "pids": pids } });
    assert!(load(&with("tiers", tier(500, 512 << 20, 128))).is_ok());
    for bad in [
        tier(0, 1 << 30, 128),
        tier(500, 0, 128),
        tier(500, 1 << 30, 0),
    ] {
        assert!(refused(&with("tiers", bad)).contains("tier"));
    }
    assert!(refused(&with("tiers", json!({}))).contains("tier"));
}

#[test]
fn paths_must_be_plain_and_absolute() {
    let cases = [
        ("socket_host_dir", "run/agentkit"),
        ("socket_host_dir", "/run/../etc"),
        ("socket_host_dir", "/"),
        ("state_dir", "/var/lib/./x"),
        ("socket_host_dir", "/run/aks/"),
        ("listen_socket", "/run//l.sock"),
        ("runtime_socket", "docker.sock"),
    ];
    for (field, path) in cases {
        let error = refused(&with(field, json!(path)));
        assert!(error.contains(field), "{field}={path}: {error}");
    }
}

#[test]
fn other_bounds_and_unknown_fields_are_refused() {
    let cases = [
        with("name_prefix", json!("AKS-")),
        with("name_prefix", json!("aks")),
        with("tmp_size_bytes", json!(0)),
        with("stop_grace_max_ms", json!(3_600_000)),
        with("max_connections", json!(0)),
        with("max_containers", json!(0)),
        with("max_containers", json!(4097)),
        with("privileged", json!(true)),
    ];
    for case in &cases {
        assert!(load(case).is_err(), "{case}");
    }
}
