use super::*;

#[test]
fn debug_redacts_secrets() {
    let auth = SearXngAuth::Basic {
        username: "u".into(),
        password: Zeroizing::new("hunter2".into()),
    };
    let text = format!("{auth:?}");
    assert!(text.contains("<redacted>") && !text.contains("hunter2"));
    let auth = SearXngAuth::Header {
        name: "X-Key".into(),
        value: Zeroizing::new("sekret".into()),
    };
    let text = format!("{auth:?}");
    assert!(text.contains("<redacted>") && !text.contains("sekret"));
}

#[test]
fn config_debug_hides_url_userinfo() {
    let cfg = SearXngConfig::new(Url::parse("https://bob:pw@h/s").unwrap());
    let text = format!("{cfg:?}");
    assert!(!text.contains("pw") && !text.contains("bob"), "{text}");
    assert!(text.contains("https://h/s"));
}

#[test]
fn validate_rejects_bad_base_urls() {
    for bad in [
        "ftp://h/",
        "https://u:p@h/",
        "https://h/?a=1",
        "https://h/#f",
    ] {
        let cfg = SearXngConfig::new(Url::parse(bad).unwrap());
        assert!(
            matches!(cfg.validate(), Err(ConfigError::InvalidBaseUrl(_))),
            "{bad}"
        );
    }
    let mut cfg = SearXngConfig::new(Url::parse("https://h/searx/").unwrap());
    assert!(cfg.validate().is_ok());
    cfg.safesearch = 3;
    assert!(matches!(
        cfg.validate(),
        Err(ConfigError::InvalidSafesearch(3))
    ));
}

#[test]
fn insecure_auth_only_allowed_on_local_hosts() {
    let auth = || SearXngAuth::Header {
        name: "X-Key".into(),
        value: Zeroizing::new("v".into()),
    };
    for (raw, good) in [
        ("http://localhost:8080/", true),
        ("http://127.0.0.1/", true),
        ("http://10.1.2.3/", true),
        ("http://172.16.0.1/", true),
        ("http://172.32.0.1/", false),
        ("http://192.168.1.5/", true),
        ("http://169.254.1.1/", true),
        ("http://[::1]/", true),
        ("http://[fd00::1]/", true),
        ("http://[2001:db8::1]/", false),
        ("http://searx.lan/", true),
        ("http://box.local/", true),
        ("http://s.internal/", true),
        ("http://s.home.arpa/", true),
        ("http://example.com/", false),
        ("http://8.8.8.8/", false),
        ("https://example.com/", true),
    ] {
        let mut cfg = SearXngConfig::new(Url::parse(raw).unwrap());
        cfg.auth = auth();
        let res = cfg.validate();
        assert_eq!(res.is_ok(), good, "{raw}");
        if !good {
            assert!(matches!(res, Err(ConfigError::InsecureAuth("searxng"))));
        }
    }
    let plain = SearXngConfig::new(Url::parse("http://example.com/").unwrap());
    assert!(plain.validate().is_ok());
    let err = ConfigError::InsecureAuth("searxng").to_string();
    assert_eq!(
        err,
        "searxng credentials would be sent unencrypted; use https or a local address."
    );
}

#[test]
fn zero_limits_rejected() {
    let ok = || WebPackConfig::new(vec![BackendConfig::Brave(BraveConfig::new("a"))]);
    assert!(ok().validate().is_ok());
    type Mutate = fn(&mut WebPackConfig);
    let cases: [(&str, Mutate); 6] = [
        ("request_timeout", |c| c.request_timeout = Duration::ZERO),
        ("total_timeout", |c| c.total_timeout = Duration::ZERO),
        ("max_body_bytes", |c| c.max_body_bytes = 0),
        ("searches_per_run", |c| c.limits.searches_per_run = 0),
        ("cache.entries_per_run", |c| c.cache.entries_per_run = 0),
        ("cache.max_runs", |c| c.cache.max_runs = 0),
    ];
    for (name, mutate) in cases {
        let mut cfg = ok();
        mutate(&mut cfg);
        assert!(
            matches!(cfg.validate(), Err(ConfigError::InvalidLimit(n)) if n == name),
            "{name}"
        );
    }
}

#[test]
fn brave_exa_debug_redacts_key() {
    let text = format!("{:?}", BraveConfig::new("brave-secret"));
    assert!(!text.contains("brave-secret") && text.contains("<redacted>"));
    let text = format!("{:?}", ExaConfig::new("exa-secret"));
    assert!(!text.contains("exa-secret") && text.contains("<redacted>"));
}

#[test]
fn keyed_config_validation() {
    assert!(BraveConfig::new("k").validate().is_ok());
    assert!(matches!(
        ExaConfig::new("  ").validate(),
        Err(ConfigError::MissingApiKey("exa"))
    ));
    for (raw, good) in [
        ("https://api.example.com/s", true),
        ("http://127.0.0.1:9/s", true),
        ("http://localhost/s", true),
        ("http://[::1]:9/s", true),
        ("http://example.com/s", false),
        ("https://u:p@example.com/s", false),
    ] {
        let mut cfg = BraveConfig::new("k");
        cfg.endpoint = Url::parse(raw).unwrap();
        assert_eq!(cfg.validate().is_ok(), good, "{raw}");
    }
}

#[test]
fn pack_validation_rejects_empty_and_duplicates() {
    let empty = WebPackConfig::new(vec![]);
    assert!(matches!(empty.validate(), Err(ConfigError::NoBackends)));
    let dup = WebPackConfig::new(vec![
        BackendConfig::Brave(BraveConfig::new("a")),
        BackendConfig::Brave(BraveConfig::new("b")),
    ]);
    assert!(matches!(
        dup.validate(),
        Err(ConfigError::DuplicateBackend("brave"))
    ));
}
