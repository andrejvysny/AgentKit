use std::collections::HashMap;

use super::*;

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |name| vars.get(name).cloned()
}

const TOKEN: &str = "s3cret-t0ken";
/// The guest supervisor's user; tests that set vsock ports never read the
/// uid they run as (the Linux suite runs as root in a container).
const AGENT_UID: u32 = 1000;

#[test]
fn defaults_fill_the_fixed_layout() {
    let config = Config::from_lookup(lookup(&[
        (GENERATION, "7"),
        (IMAGE_DIGEST, "sha256:abc"),
        (CHANNEL_TOKEN, TOKEN),
    ]))
    .unwrap();
    assert_eq!(config.generation, 7);
    assert_eq!(config.image_digest, "sha256:abc");
    assert_eq!(config.channel_token, TOKEN);
    assert_eq!(
        config.control_socket,
        PathBuf::from("/run/agentkit/control.sock")
    );
    assert_eq!(
        config.egress_socket,
        PathBuf::from("/run/agentkit/egress.sock")
    );
    assert_eq!(config.workspace_root, PathBuf::from("/workspace"));
    assert_eq!(config.home_root, PathBuf::from("/home/agent"));
    assert_eq!(config.shim_addr.to_string(), "127.0.0.1:3128");
    assert_eq!(config.mode, ProcessMode::Supervisor, "tests are not PID 1");
}

#[test]
fn socket_paths_are_overridable_but_must_be_absolute() {
    let config = Config::from_lookup(lookup(&[
        (GENERATION, "1"),
        (IMAGE_DIGEST, "d"),
        (CHANNEL_TOKEN, TOKEN),
        (CONTROL_SOCKET, "/tmp/c.sock"),
        (EGRESS_SOCKET, "/tmp/e.sock"),
    ]))
    .unwrap();
    assert_eq!(config.control_socket, PathBuf::from("/tmp/c.sock"));
    assert_eq!(config.egress_socket, PathBuf::from("/tmp/e.sock"));
    let relative = Config::from_lookup(lookup(&[
        (GENERATION, "1"),
        (IMAGE_DIGEST, "d"),
        (CHANNEL_TOKEN, TOKEN),
        (CONTROL_SOCKET, "c.sock"),
    ]));
    assert!(matches!(
        relative,
        Err(ConfigError::Invalid {
            name: CONTROL_SOCKET,
            ..
        })
    ));
}

#[test]
fn missing_or_bad_values_are_refused() {
    assert_eq!(
        Config::from_lookup(lookup(&[(IMAGE_DIGEST, "d")])).unwrap_err(),
        ConfigError::Missing(GENERATION)
    );
    assert_eq!(
        Config::from_lookup(lookup(&[(GENERATION, "1")])).unwrap_err(),
        ConfigError::Missing(IMAGE_DIGEST)
    );
    assert_eq!(
        Config::from_lookup(lookup(&[(GENERATION, "1"), (IMAGE_DIGEST, "d")])).unwrap_err(),
        ConfigError::Missing(CHANNEL_TOKEN)
    );
    for generation in ["-1", "x", "", "18446744073709551616"] {
        let refused = Config::from_lookup(lookup(&[(GENERATION, generation), (IMAGE_DIGEST, "d")]));
        assert!(
            matches!(
                refused,
                Err(ConfigError::Invalid {
                    name: GENERATION,
                    ..
                })
            ),
            "{generation:?}"
        );
    }
    let long = "a".repeat(IMAGE_DIGEST_MAX_BYTES + 1);
    for digest in ["", "has space", long.as_str()] {
        let refused = Config::from_lookup(lookup(&[
            (GENERATION, "1"),
            (IMAGE_DIGEST, digest),
            (CHANNEL_TOKEN, TOKEN),
        ]));
        assert!(
            matches!(
                refused,
                Err(ConfigError::Invalid {
                    name: IMAGE_DIGEST,
                    ..
                })
            ),
            "{digest:?}"
        );
    }
}

#[test]
fn the_channel_token_is_required_and_never_printed() {
    let long = "t".repeat(CHANNEL_TOKEN_MAX_BYTES + 1);
    for token in ["", "has space", "new\nline", long.as_str()] {
        let refused = Config::from_lookup(lookup(&[
            (GENERATION, "1"),
            (IMAGE_DIGEST, "d"),
            (CHANNEL_TOKEN, token),
        ]));
        let error = refused.unwrap_err();
        assert_eq!(
            error,
            ConfigError::Invalid {
                name: CHANNEL_TOKEN,
                reason: "not 1 to 256 printable ASCII characters",
            },
            "{token:?}"
        );
        assert!(token.is_empty() || !error.to_string().contains(token));
    }
    let config = Config::from_lookup(lookup(&[
        (GENERATION, "1"),
        (IMAGE_DIGEST, "d"),
        (CHANNEL_TOKEN, TOKEN),
    ]))
    .unwrap();
    let printed = format!("{config:?} {config:#?}");
    assert!(!printed.contains(TOKEN), "{printed}");
    assert!(printed.contains("<redacted>"));
}

#[test]
fn without_vsock_ports_both_links_are_the_unix_sockets() {
    for vars in [
        &[][..],
        &[
            (CONTROL_SOCKET, "/tmp/c.sock"),
            (EGRESS_SOCKET, "/tmp/e.sock"),
        ][..],
    ] {
        let transport = Transport::from_lookup(lookup(vars)).unwrap();
        assert_eq!(transport, Transport::default(), "{vars:?}");
        assert_eq!(
            (transport.control, transport.egress),
            (Link::Socket, Link::Socket)
        );
    }
}

#[test]
fn vsock_ports_parse_as_unsigned_32_bit_numbers() {
    for (value, port) in [("1024", 1024), ("0", 0), ("4294967294", u32::MAX - 1)] {
        let pairs = [(CONTROL_VSOCK_PORT, value)];
        let vars = lookup(&pairs);
        assert_eq!(
            vsock_port(&vars, CONTROL_VSOCK_PORT, CONTROL_SOCKET),
            Ok(Some(port)),
            "{value:?}"
        );
    }
    let none: [(&str, &str); 0] = [];
    let unset = lookup(&none);
    assert_eq!(
        vsock_port(&unset, EGRESS_VSOCK_PORT, EGRESS_SOCKET),
        Ok(None)
    );
}

#[test]
fn bad_vsock_ports_are_refused() {
    let bad = ["", "x", "-1", " 1024", "1024 ", "4294967295", "4294967296"];
    for (name, value) in [CONTROL_VSOCK_PORT, EGRESS_VSOCK_PORT]
        .into_iter()
        .flat_map(|name| bad.map(|value| (name, value)))
    {
        let refused = Transport::from_lookup(lookup(&[(name, value)]));
        assert_eq!(
            refused,
            Err(ConfigError::Invalid {
                name,
                reason: "not a vsock port (0 to 4294967294)",
            }),
            "{name}={value:?}"
        );
    }
}

#[test]
fn a_vsock_port_conflicts_with_the_socket_of_its_link() {
    let control = Transport::from_lookup(lookup(&[
        (CONTROL_VSOCK_PORT, "1024"),
        (CONTROL_SOCKET, "/tmp/c.sock"),
    ]));
    assert_eq!(
        control,
        Err(ConfigError::Conflict {
            name: CONTROL_VSOCK_PORT,
            with: CONTROL_SOCKET,
        })
    );
    let egress = Transport::from_lookup(lookup(&[
        (EGRESS_VSOCK_PORT, "1025"),
        (EGRESS_SOCKET, "/tmp/e.sock"),
    ]));
    assert_eq!(
        egress,
        Err(ConfigError::Conflict {
            name: EGRESS_VSOCK_PORT,
            with: EGRESS_SOCKET,
        })
    );
    assert_eq!(
        control.unwrap_err().to_string(),
        "AGENTKIT_CONTROL_VSOCK_PORT and AGENTKIT_CONTROL_SOCKET are both set"
    );
    // The other link's socket is no conflict.
    let pairs = [(CONTROL_VSOCK_PORT, "1024"), (EGRESS_SOCKET, "/tmp/e.sock")];
    let mixed = lookup(&pairs);
    assert_eq!(
        vsock_port(&mixed, CONTROL_VSOCK_PORT, CONTROL_SOCKET),
        Ok(Some(1024))
    );
}

#[test]
fn a_vsock_link_is_refused_as_root() {
    for name in [CONTROL_VSOCK_PORT, EGRESS_VSOCK_PORT] {
        let pairs = [(name, "1024")];
        assert_eq!(
            parse(&lookup(&pairs), ROOT_UID),
            Err(ConfigError::Root(name)),
            "{name}"
        );
    }
    let both = [(CONTROL_VSOCK_PORT, "1024"), (EGRESS_VSOCK_PORT, "1025")];
    let refused = parse(&lookup(&both), ROOT_UID).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "AGENTKIT_CONTROL_VSOCK_PORT is set but the supervisor runs as root; a microVM guest must start it as an unprivileged user"
    );
    let sockets = [(CONTROL_SOCKET, "/tmp/c.sock")];
    assert_eq!(
        parse(&lookup(&sockets), ROOT_UID),
        Ok(Transport::default()),
        "a container's supervisor may run as root"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn vsock_is_refused_off_linux() {
    for name in [CONTROL_VSOCK_PORT, EGRESS_VSOCK_PORT] {
        let refused = parse(&lookup(&[(name, "1024")]), AGENT_UID);
        assert_eq!(refused, Err(ConfigError::Unsupported(name)));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_ports_select_vsock_links_one_by_one() {
    let both = parse(
        &lookup(&[(CONTROL_VSOCK_PORT, "1024"), (EGRESS_VSOCK_PORT, "1025")]),
        AGENT_UID,
    )
    .unwrap();
    assert_eq!(
        (both.control, both.egress),
        (Link::Vsock(1024), Link::Vsock(1025))
    );
    let control_only = parse(
        &lookup(&[(CONTROL_VSOCK_PORT, "1024"), (EGRESS_SOCKET, "/tmp/e.sock")]),
        AGENT_UID,
    )
    .unwrap();
    assert_eq!(
        (control_only.control, control_only.egress),
        (Link::Vsock(1024), Link::Socket)
    );
    let egress_only = parse(&lookup(&[(EGRESS_VSOCK_PORT, "1025")]), AGENT_UID).unwrap();
    assert_eq!(
        (egress_only.control, egress_only.egress),
        (Link::Socket, Link::Vsock(1025))
    );
}
