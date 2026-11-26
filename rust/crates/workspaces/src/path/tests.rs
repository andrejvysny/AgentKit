use super::*;

fn ok(text: &str) -> String {
    RelPath::parse(text)
        .unwrap_or_else(|error| panic!("{text:?}: {error}"))
        .as_str()
        .to_owned()
}

#[test]
fn root_forms_are_the_root() {
    for text in ["", ".", "./", "./.", ".//", "a/.."] {
        let path = RelPath::parse(text).unwrap();
        assert!(path.is_root(), "{text:?}");
        assert_eq!(path.to_string(), ".");
    }
    assert_eq!(RelPath::root(), RelPath::parse(".").unwrap());
}

#[test]
fn normalises_dots_and_repeated_slashes() {
    assert_eq!(ok("a/b/c"), "a/b/c");
    assert_eq!(ok("./a//b/./c/"), "a/b/c");
    assert_eq!(ok("a/../b"), "b");
    assert_eq!(ok("a/b/../../c"), "c");
    assert_eq!(ok("chats/c1/input/data.csv"), "chats/c1/input/data.csv");
}

#[test]
fn keeps_names_that_only_look_special() {
    assert_eq!(ok("..."), "...");
    assert_eq!(ok("..a/b.."), "..a/b..");
    assert_eq!(ok("~/x"), "~/x");
    assert_eq!(ok("a\\b"), "a\\b");
    assert_eq!(ok(" spaced name "), " spaced name ");
}

#[test]
fn refuses_absolute_paths() {
    for text in ["/", "/etc/passwd", "/workspace/a"] {
        assert_eq!(
            RelPath::parse(text),
            Err(RelPathError::Absolute),
            "{text:?}"
        );
    }
}

#[test]
fn refuses_escaping_parent_segments() {
    for text in [
        "..",
        "../a",
        "a/../..",
        "a/../../b",
        "./../x",
        "a/b/../../../c",
    ] {
        assert_eq!(
            RelPath::parse(text),
            Err(RelPathError::EscapesRoot),
            "{text:?}"
        );
    }
}

#[test]
fn refuses_nul_anywhere() {
    for text in ["\0", "a\0b", "a/\0", "/\0"] {
        assert_eq!(RelPath::parse(text), Err(RelPathError::Nul), "{text:?}");
    }
}

#[test]
fn enforces_length_limits() {
    let segment = "s".repeat(REL_PATH_SEGMENT_MAX_BYTES);
    assert!(RelPath::parse(&segment).is_ok());
    let long_segment = "s".repeat(REL_PATH_SEGMENT_MAX_BYTES + 1);
    assert_eq!(
        RelPath::parse(&long_segment),
        Err(RelPathError::SegmentTooLong(REL_PATH_SEGMENT_MAX_BYTES + 1))
    );
    let deep = vec!["d".repeat(100); 50].join("/");
    assert!(matches!(
        RelPath::parse(&deep),
        Err(RelPathError::TooLong(_))
    ));
}

#[test]
fn join_resolves_below_the_base() {
    let base = RelPath::parse("chats/c1").unwrap();
    let file = RelPath::parse("out/report.md").unwrap();
    assert_eq!(base.join(&file).unwrap().as_str(), "chats/c1/out/report.md");
    assert_eq!(RelPath::root().join(&file).unwrap(), file);
    assert_eq!(base.join(&RelPath::root()).unwrap(), base);
}

#[test]
fn starts_with_matches_whole_segments() {
    let base = RelPath::parse("chats/c1").unwrap();
    assert!(RelPath::parse("chats/c1/a").unwrap().starts_with(&base));
    assert!(base.starts_with(&base));
    assert!(!RelPath::parse("chats/c10").unwrap().starts_with(&base));
    assert!(base.starts_with(&RelPath::root()));
}

#[test]
fn serde_validates_and_normalises() {
    let path: RelPath = serde_json::from_str("\"./a//b\"").unwrap();
    assert_eq!(path.as_str(), "a/b");
    assert_eq!(serde_json::to_string(&path).unwrap(), "\"a/b\"");
    assert!(serde_json::from_str::<RelPath>("\"../x\"").is_err());
    assert!(serde_json::from_str::<RelPath>("\"/x\"").is_err());
}
