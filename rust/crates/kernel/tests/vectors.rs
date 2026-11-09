use agentkit_kernel::{canonical_json, canonical_sha256, sha256_hex};
use serde_json::json;

#[test]
fn sha256_hex_of_empty_input() {
    assert_eq!(
        sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_hex_accepts_str_and_bytes() {
    assert_eq!(sha256_hex("abc"), sha256_hex(b"abc"));
    assert_eq!(
        sha256_hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn canonical_json_sorts_nested_keys_and_keeps_array_order() {
    let value = json!({"b": {"z": 1, "a": [3, 1, 2]}, "a": true});
    assert_eq!(
        canonical_json(&value),
        r#"{"a":true,"b":{"a":[3,1,2],"z":1}}"#
    );
}

#[test]
fn canonical_json_spells_numbers_like_javascript() {
    assert_eq!(
        canonical_json(&json!({"x": [1.0, 1e-7, 1e21]})),
        r#"{"x":[1,1e-7,1e+21]}"#
    );
}

#[test]
fn canonical_sha256_hashes_the_canonical_text() {
    let value = json!({"b": 1, "a": 2});
    assert_eq!(canonical_sha256(&value), sha256_hex(r#"{"a":2,"b":1}"#));
}
