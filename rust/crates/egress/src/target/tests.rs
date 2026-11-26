use std::net::IpAddr;

use super::{Target, TargetError};

#[test]
fn names_are_lowercased_and_lose_one_trailing_dot() {
    let target = Target::new("PyPI.Org.", 443).expect("valid");
    assert_eq!(target.host(), "pypi.org");
    assert_eq!(target.to_string(), "pypi.org:443");
    assert_eq!(target, Target::new("pypi.org", 443).expect("valid"));
    assert_eq!(target.literal(), None);
}

#[test]
fn invalid_names_are_refused() {
    let cases = [
        ("", TargetError::EmptyHost),
        (".", TargetError::EmptyHost),
        ("*.example.com", TargetError::Wildcard),
        ("a_b.example", TargetError::InvalidCharacter('_')),
        ("caf\u{e9}.example", TargetError::InvalidCharacter('\u{e9}')),
        ("a..b", TargetError::InvalidLabel(String::new())),
        ("-a.example", TargetError::InvalidLabel("-a".into())),
        ("a.example..", TargetError::InvalidLabel(String::new())),
        ("[::1%en0]", TargetError::InvalidAddress),
        ("[example.com]", TargetError::InvalidAddress),
        ("[::1", TargetError::InvalidAddress),
        ("::1", TargetError::InvalidCharacter(':')),
    ];
    for (host, want) in cases {
        assert_eq!(Target::new(host, 443), Err(want), "{host:?}");
    }
    let long = format!("{}.example", "a".repeat(250));
    assert_eq!(Target::new(&long, 443), Err(TargetError::HostTooLong));
    let label = format!("{}.example", "a".repeat(64));
    assert!(matches!(
        Target::new(&label, 443),
        Err(TargetError::InvalidLabel(_))
    ));
    assert_eq!(Target::new("example.com", 0), Err(TargetError::PortZero));
}

#[test]
fn literals_are_canonical_and_known() {
    let v4 = Target::new("93.184.216.34", 443).expect("valid");
    assert_eq!(v4.literal(), Some(IpAddr::from([93, 184, 216, 34])));
    let v6 = Target::new("[2001:DB8:0:0::1]", 443).expect("valid");
    assert_eq!(v6.host(), "[2001:db8::1]");
    assert_eq!(v6, Target::new("[2001:db8::1]", 443).expect("valid"));
    assert_eq!(v6.literal(), Some("2001:db8::1".parse().expect("addr")));
    // Not a canonical IPv4 literal: a name, resolved (and still checked).
    assert_eq!(Target::new("127.1", 80).expect("valid").literal(), None);
}
