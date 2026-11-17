//! The invariant map cannot rot: every owning test it names still exists, and
//! the README table is the map.

use agentkit_eval::coverage::{OWNERS, markdown_table, verify};

#[test]
fn every_owning_test_still_exists() {
    let missing: Vec<String> = OWNERS
        .iter()
        .filter_map(|owner| verify(owner).err())
        .collect();
    assert!(missing.is_empty(), "{missing:#?}");
}

#[test]
fn every_suite_has_an_owner_or_is_owned_here() {
    for suite in [
        "tool_pipeline",
        "context",
        "memory",
        "delegation",
        "accounting",
        "oracle_integrity",
    ] {
        assert!(
            OWNERS.iter().any(|owner| owner.suite == suite),
            "{suite} maps to no existing test"
        );
    }
}

#[test]
fn the_readme_table_is_the_map() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("README.md");
    assert!(
        readme.contains(&markdown_table()),
        "README.md's invariant table differs from agentkit_eval::coverage::OWNERS; \
         regenerate it from markdown_table()"
    );
}
