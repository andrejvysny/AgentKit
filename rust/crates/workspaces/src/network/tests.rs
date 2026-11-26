use super::*;
use serde_json::json;

struct Counter(std::sync::atomic::AtomicU64);
impl Runtime for Counter {
    fn now_ms(&self) -> i64 {
        0
    }
    fn id(&self) -> String {
        let next = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("manual-{next}")
    }
}

#[test]
fn host_port_accepts_names_and_lowercases() {
    let target = HostPort::new("PyPI.org", 443).unwrap();
    assert_eq!(target.host(), "pypi.org");
    assert_eq!(target.to_string(), "pypi.org:443");
    assert!(HostPort::new("10.0.0.1", 8080).is_ok());
    assert!(HostPort::new("a-b.example", 65535).is_ok());
}

#[test]
fn host_port_refuses_wildcards_and_bad_spelling() {
    let cases = [
        ("*.example.com", HostPortError::Wildcard),
        ("", HostPortError::EmptyHost),
        ("exa mple.com", HostPortError::InvalidCharacter(' ')),
        ("::1", HostPortError::InvalidCharacter(':')),
        ("a_b.com", HostPortError::InvalidCharacter('_')),
        ("example.com.", HostPortError::InvalidLabel(String::new())),
        (".example.com", HostPortError::InvalidLabel(String::new())),
        ("-a.com", HostPortError::InvalidLabel("-a".into())),
    ];
    for (host, error) in cases {
        assert_eq!(HostPort::new(host, 443), Err(error), "{host:?}");
    }
    assert_eq!(HostPort::new("a.com", 0), Err(HostPortError::PortZero));
    let long = format!("{}.com", "a".repeat(64));
    assert!(matches!(
        HostPort::new(&long, 443),
        Err(HostPortError::InvalidLabel(_))
    ));
    let too_long = vec!["a".repeat(60); 5].join(".");
    assert_eq!(
        HostPort::new(&too_long, 443),
        Err(HostPortError::HostTooLong)
    );
}

#[test]
fn host_port_validates_on_deserialize() {
    let target: HostPort = serde_json::from_value(json!({"host": "A.com", "port": 80})).unwrap();
    assert_eq!(target.host(), "a.com");
    assert!(serde_json::from_value::<HostPort>(json!({"host": "*", "port": 80})).is_err());
    assert!(serde_json::from_value::<HostPort>(json!({"host": "a.com", "port": 0})).is_err());
}

#[test]
fn default_grants_cover_the_registries() {
    let runtime = Counter(0.into());
    let workspace = WorkspaceId::new("w1");
    let grants = default_grants(&workspace, &runtime, 5);
    let targets: Vec<String> = grants.iter().map(|g| g.target.to_string()).collect();
    assert_eq!(
        targets,
        [
            "pypi.org:443",
            "files.pythonhosted.org:443",
            "registry.npmjs.org:443"
        ]
    );
    assert!(
        grants
            .iter()
            .all(|g| g.source == GrantSource::Default && g.is_active())
    );
    assert_ne!(grants[0].id, grants[1].id);
}

#[test]
fn grant_flattens_its_target_on_the_wire() {
    let runtime = Counter(0.into());
    let grant = default_grants(&WorkspaceId::new("w1"), &runtime, 5).remove(0);
    let value = serde_json::to_value(&grant).unwrap();
    assert_eq!(value["host"], "pypi.org");
    assert_eq!(value["port"], 443);
    assert_eq!(value["workspaceId"], "w1");
    let back: NetworkGrant = serde_json::from_value(value).unwrap();
    assert_eq!(back, grant);
}

#[test]
fn policy_is_the_active_grants_only() {
    let runtime = Counter(0.into());
    let mut grants = default_grants(&WorkspaceId::new("w1"), &runtime, 5);
    grants[0].revoked_at = Some(6);
    let policy = NetworkPolicy::from_grants(&grants);
    assert_eq!(policy.grants.len(), 2);
    assert!(!policy.allows(&HostPort::new("pypi.org", 443).unwrap()));
    assert!(policy.allows(&HostPort::new("registry.npmjs.org", 443).unwrap()));
    assert!(NetworkPolicy::default().grants.is_empty());
}
