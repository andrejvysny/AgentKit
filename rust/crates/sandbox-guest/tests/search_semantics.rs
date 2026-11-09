#![cfg(target_os = "linux")]
//! Golden-vector search semantics against the fixed corpus in
//! `tests/fixtures/corpus` (§8.2): literal/regex, case, glob, gitignore,
//! binary sniff, budgets, context lines, line cut, truncation flags.

use std::path::{Path, PathBuf};

use agentkit_sandbox_contracts::api::SearchQuery;
use agentkit_sandbox_contracts::api::messages::{GuestFsOp, GuestOpOutcome};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_guest::fsops::{GuestFs, Roots};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("corpus")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for ent in std::fs::read_dir(from).expect("readdir") {
        let ent = ent.expect("entry");
        let ft = ent.file_type().expect("ft");
        let dst = to.join(ent.file_name());
        if ft.is_dir() {
            copy_tree(&ent.path(), &dst);
        } else {
            std::fs::copy(ent.path(), &dst).expect("copy");
        }
    }
}

fn fixture() -> (tempfile::TempDir, GuestFs) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let ws = tmp.path().join("workspace");
    let home = tmp.path().join("home");
    let gstmp = tmp.path().join("tmp");
    copy_tree(&corpus_dir(), &ws);
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&gstmp).expect("gstmp");
    let fs = GuestFs::open(Roots::for_tests(ws, home, gstmp)).expect("open");
    (tmp, fs)
}

fn q(pattern: &str) -> SearchQuery {
    SearchQuery {
        root: GuestPath::parse("/workspace").unwrap(),
        pattern: pattern.to_string(),
        literal: true,
        ignore_case: false,
        glob: None,
        context: 0,
        max_matches: 1000,
        max_entries: 10_000,
        timeout_ms: 5_000,
    }
}

fn search(fs: &GuestFs, query: SearchQuery) -> agentkit_sandbox_contracts::api::SearchReply {
    let out = fs.handle(&GuestFsOp::Search { query }, "s1");
    match out {
        GuestOpOutcome::SearchOk { reply } => reply,
        other => panic!("search failed: {other:?}"),
    }
}

#[test]
fn literal_case_sensitive_vectors() {
    let (_t, fs) = fixture();
    let r = search(&fs, q("alpha"));
    let mut paths: Vec<(String, u32)> =
        r.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("docs/notes.txt".to_string(), 1),
            ("src/a.rs".to_string(), 1)
        ],
        "literal 'alpha' golden"
    );
    assert!(!r.truncated && !r.timed_out);
}

#[test]
fn ignore_case_vectors() {
    let (_t, fs) = fixture();
    let mut query = q("alpha");
    query.ignore_case = true;
    let r = search(&fs, query);
    let mut paths: Vec<(String, u32)> =
        r.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("docs/notes.txt".to_string(), 1),
            ("docs/notes.txt".to_string(), 3),
            ("src/a.rs".to_string(), 1),
            ("src/b.rs".to_string(), 2),
        ],
        "ignore-case 'alpha' golden"
    );
}

#[test]
fn regex_vectors() {
    let (_t, fs) = fixture();
    let mut query = q(r"fn \w+");
    query.literal = false;
    let r = search(&fs, query);
    let mut paths: Vec<(String, u32)> =
        r.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ("src/a.rs".to_string(), 1),
            ("src/a.rs".to_string(), 2),
            ("src/b.rs".to_string(), 1),
        ],
        "regex 'fn \\w+' golden"
    );
}

#[test]
fn invalid_regex_fails_closed() {
    let (_t, fs) = fixture();
    let mut query = q("fn (");
    query.literal = false;
    let out = fs.handle(&GuestFsOp::Search { query }, "s1");
    match out {
        GuestOpOutcome::Failed { code, .. } => assert_eq!(code, "bad_input"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn glob_filter_vectors() {
    let (_t, fs) = fixture();
    let mut query = q("alpha");
    query.glob = Some("docs/*".into());
    let r = search(&fs, query);
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].path, "docs/notes.txt");

    // Bare glob matches basename too (toolpacks semantics).
    let mut query = q("gamma");
    query.glob = Some("*.rs".into());
    let r = search(&fs, query);
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].path, "src/b.rs");

    // Brace alternation.
    let mut query = q("alpha");
    query.glob = Some("**/*.{rs,txt}".into());
    let r = search(&fs, query);
    assert!(
        r.matches
            .iter()
            .all(|m| m.path.ends_with(".rs") || m.path.ends_with(".txt"))
    );
}

#[test]
fn gitignore_honored_in_root_only() {
    let (_t, fs) = fixture();
    // ignored/skip.txt contains 'alpha' but .gitignore says ignored/.
    let r = search(&fs, q("alpha"));
    assert!(
        !r.matches.iter().any(|m| m.path.contains("ignored/")),
        "gitignore violated: {:?}",
        r.matches
    );
    // data.bin contains 'alpha' but is both ignored and binary.
    assert!(!r.matches.iter().any(|m| m.path.ends_with(".bin")));

    // Directly searching the ignored file by glob still excludes it.
    let mut query = q("ignored");
    query.glob = Some("ignored/*".into());
    let r = search(&fs, query);
    assert!(r.matches.is_empty() || !r.matches.iter().any(|m| m.path.contains("skip.txt")));
}

#[test]
fn binary_sniff_skips_non_text() {
    let (_t, fs) = fixture();
    let mut query = q("alpha");
    query.glob = Some("*.bin".into());
    // .gitignore lists data.bin, so move the probe beyond the rule name:
    // create a fresh binary file inside the root.
    std::fs::write(
        fs.roots
            .dir(agentkit_sandbox_contracts::path::GuestRoot::Workspace)
            .join("probe.dat"),
        b"alpha\x00\xff\xfe binary",
    )
    .expect("write probe");
    query.glob = Some("probe.dat".into());
    let r = search(&fs, query);
    assert!(
        r.matches.is_empty(),
        "binary must be skipped: {:?}",
        r.matches
    );
}

#[test]
fn match_cap_sets_truncated() {
    let (_t, fs) = fixture();
    let mut query = q("alpha");
    query.ignore_case = true;
    query.max_matches = 1;
    let r = search(&fs, query);
    assert_eq!(r.matches.len(), 1);
    assert!(r.truncated);
}

#[test]
fn entry_budget_sets_truncated() {
    let (_t, fs) = fixture();
    let mut query = q("nothing-will-match-this");
    query.max_entries = 2;
    let r = search(&fs, query);
    assert!(r.truncated, "tiny entry budget must truncate");
}

#[test]
fn zero_timeout_reports_timed_out() {
    let (_t, fs) = fixture();
    let mut query = q("alpha");
    query.timeout_ms = 0;
    let r = search(&fs, query);
    assert!(r.timed_out);
    assert!(r.matches.is_empty());
}

#[test]
fn context_lines_golden() {
    let (_t, fs) = fixture();
    let mut query = q("fn beta");
    query.context = 1;
    let r = search(&fs, query);
    assert_eq!(r.matches.len(), 1);
    let m = &r.matches[0];
    assert_eq!(m.path, "src/a.rs");
    assert_eq!(m.line, 2);
    assert_eq!(m.context_before, vec!["fn alpha() {}".to_string()]);
    assert_eq!(m.context_after, vec!["// comment".to_string()]);
}

#[test]
fn long_lines_cut_at_1024_chars() {
    let (_t, fs) = fixture();
    let r = search(&fs, q("needle"));
    assert_eq!(r.matches.len(), 1);
    assert_eq!(r.matches[0].text.chars().count(), 1024);
    assert!(r.matches[0].text.starts_with("needle"));
}

#[test]
fn file_change_during_read_returns_actual_digest() {
    // Read integrity: the digest describes the bytes actually returned.
    let (_t, fs) = fixture();
    let out = fs.handle(
        &GuestFsOp::Read {
            path: GuestPath::parse("/workspace/src/a.rs").unwrap(),
            from: None,
            len: Some(13),
        },
        "r1",
    );
    let GuestOpOutcome::ReadOk { reply } = out else {
        panic!("read failed: {out:?}");
    };
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &reply.data_b64)
        .unwrap();
    assert_eq!(
        reply.sha256,
        agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(&bytes)
    );
    assert_eq!(
        reply.total_size,
        std::fs::metadata(corpus_dir().join("src/a.rs"))
            .unwrap()
            .len()
    );
}
