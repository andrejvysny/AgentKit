use super::*;

const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// A macOS `$TMPDIR` as `getconf DARWIN_USER_TEMP_DIR` prints it (49
/// bytes), with the ADR's `aksbx` socket root below it.
const PRODUCTION_SOCKET_ROOT: &str = "/var/folders/z5/x_0nvl9n33nd463_mpr617p00000gn/T/aksbx";

fn image(path: &str) -> ImageFile {
    ImageFile {
        path: path.into(),
        sha256: SHA.to_owned(),
    }
}

fn config(socket_root: &str) -> VmLauncherConfig {
    VmLauncherConfig::new(
        "/Users/someone/Library/Application Support/OneMind/sandbox".into(),
        socket_root.into(),
        "/Applications/OneMind.app/Contents/MacOS/agentkit-sandbox-vmhelper".into(),
        image("/Applications/OneMind.app/Contents/Resources/vm/Image"),
        image("/Applications/OneMind.app/Contents/Resources/vm/root.ext4"),
    )
}

#[test]
fn the_production_layout_is_valid() {
    let config = config(PRODUCTION_SOCKET_ROOT);
    config.validate().unwrap();
    assert_eq!(config.image_digest(), format!("sha256:{SHA}"));
    assert_eq!(
        config.listen_socket(),
        Path::new(PRODUCTION_SOCKET_ROOT).join("launcher.sock")
    );
}

#[test]
fn production_machine_sockets_fit_the_macos_limit() {
    let machine = agentkit_sandbox_proto::machine_dir_name(&"w".repeat(64), u64::MAX);
    let path = format!("{PRODUCTION_SOCKET_ROOT}/{machine}/{CONTROL_SOCKET}");
    assert!(path.len() < 104, "{} bytes", path.len());
    assert!(sockets_fit(Path::new(PRODUCTION_SOCKET_ROOT), 104));
}

#[test]
fn a_socket_root_too_long_for_sun_path_is_refused() {
    // 65 bytes is the longest root whose machine sockets fit 104 bytes.
    let longest = format!("/{}", "d".repeat(64));
    assert!(sockets_fit(Path::new(&longest), 104));
    assert!(!sockets_fit(Path::new(&format!("{longest}x")), 104));
    // The production root with `/private` in front (the resolved `/var`)
    // still fits; twelve more bytes do not.
    let resolved = format!("/private{PRODUCTION_SOCKET_ROOT}");
    assert!(sockets_fit(Path::new(&resolved), 104));
    let long = format!("{resolved}-{}", "x".repeat(11));
    assert!(!sockets_fit(Path::new(&long), 104));
    let refused = config(&format!("/{}", "d".repeat(SUN_PATH_BYTES)));
    assert!(matches!(
        refused.validate(),
        Err(VmConfigError::Invalid(why)) if why.contains("socket_root is too long")
    ));
}

#[test]
fn paths_must_be_plain_and_absolute() {
    for bad in ["relative/root", "/a/../b", "/a/./b", "/a//b", "/a/b/", "/"] {
        let mut bad_config = config(PRODUCTION_SOCKET_ROOT);
        bad_config.data_root = bad.into();
        assert!(bad_config.validate().is_err(), "{bad}");
    }
}

#[test]
fn digests_sizes_and_bounds_are_checked() {
    let mut cases = Vec::new();
    let mut digest = config(PRODUCTION_SOCKET_ROOT);
    digest.root_disk.sha256 = "ABC".into();
    cases.push(digest);
    let mut small = config(PRODUCTION_SOCKET_ROOT);
    small.workspace_disk_bytes = 512 * MIB;
    cases.push(small);
    let mut odd = config(PRODUCTION_SOCKET_ROOT);
    odd.home_disk_bytes = 4 * GIB + 1;
    cases.push(odd);
    let mut machines = config(PRODUCTION_SOCKET_ROOT);
    machines.max_machines = 0;
    cases.push(machines);
    let mut timeout = config(PRODUCTION_SOCKET_ROOT);
    timeout.connect_timeout = Duration::ZERO;
    cases.push(timeout);
    for bad in cases {
        assert!(matches!(bad.validate(), Err(VmConfigError::Invalid(_))));
    }
}

#[test]
fn tiers_map_to_whole_vcpus_by_value() {
    let mut config = config(PRODUCTION_SOCKET_ROOT);
    let half = Tier {
        cpus_milli: 1500,
        memory_bytes: GIB,
        pids: 128,
    };
    config.tiers.insert("half".into(), half);
    config.validate().unwrap();
    let shape = config.shape_of(&half).unwrap();
    assert_eq!((shape.cpus, shape.memory_bytes), (2, GIB));
    let other = Tier { pids: 129, ..half };
    assert_eq!(config.shape_of(&other), None, "tiers match by value");
    let mut huge = config.clone();
    huge.tiers.insert(
        "huge".into(),
        Tier {
            cpus_milli: 64_001,
            ..half
        },
    );
    assert!(huge.validate().is_err());
    let mut unaligned = config;
    unaligned.tiers.insert(
        "odd".into(),
        Tier {
            memory_bytes: GIB + 1,
            ..half
        },
    );
    assert!(unaligned.validate().is_err());
}
