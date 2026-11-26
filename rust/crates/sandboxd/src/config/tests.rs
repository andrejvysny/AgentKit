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
