use super::*;

#[test]
fn workspace_ids() {
    let longest = "a".repeat(WORKSPACE_ID_MAX_BYTES);
    for ok in ["a", "0", "ws-1", "019a-7c3e-uuid", longest.as_str()] {
        assert!(valid_workspace_id(ok), "{ok:?}");
    }
    let too_long = "a".repeat(WORKSPACE_ID_MAX_BYTES + 1);
    for bad in [
        "",
        "A",
        "a_b",
        "a/b",
        "..",
        "a.b",
        "é",
        "a b",
        too_long.as_str(),
    ] {
        assert!(!valid_workspace_id(bad), "{bad:?}");
    }
}

#[test]
fn operation_ids() {
    let longest = "x".repeat(OPERATION_ID_MAX_BYTES);
    for ok in [
        "op1",
        "019a7c3e-0000-7000-8000-000000000000",
        "a.b",
        "run:1_2",
        longest.as_str(),
    ] {
        assert!(valid_operation_id(ok), "{ok:?}");
    }
    let too_long = "x".repeat(OPERATION_ID_MAX_BYTES + 1);
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "a/b",
        "a\0b",
        "a b",
        "ü",
        too_long.as_str(),
    ] {
        assert!(!valid_operation_id(bad), "{bad:?}");
    }
}

#[test]
fn sha256_digests() {
    assert!(valid_sha256(&"0a".repeat(32)));
    assert!(!valid_sha256(&"0A".repeat(32)));
    assert!(!valid_sha256(&"0a".repeat(31)));
    assert!(!valid_sha256(&"g".repeat(64)));
}

#[test]
fn ledger_execution_ids_are_operation_ids() {
    // The ledger's execution id is a canonical SHA-256 in hex.
    let id = "0123456789abcdef".repeat(4);
    assert!(valid_sha256(&id));
    assert!(valid_operation_id(&id));
}

#[test]
fn machine_dir_names_are_fixed_length_and_distinct() {
    let a = super::machine_dir_name("ws-1", 1);
    assert_eq!(a.len(), super::MACHINE_DIR_NAME_BYTES);
    assert!(a.starts_with('m') && a[1..].bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(a, super::machine_dir_name("ws-1", 1));
    assert_ne!(a, super::machine_dir_name("ws-1", 2));
    assert_ne!(a, super::machine_dir_name("ws-11", 1));
    let long = "a".repeat(super::WORKSPACE_ID_MAX_BYTES);
    assert_eq!(
        super::machine_dir_name(&long, u64::MAX).len(),
        super::MACHINE_DIR_NAME_BYTES
    );
}
