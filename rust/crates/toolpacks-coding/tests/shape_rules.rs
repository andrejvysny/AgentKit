//! Roadmap 2.2: one test group per shaping rule, run on hand-written,
//! realistic outputs under `tests/fixtures/shape/<tool>/`. Each rule must
//! save at least 20% (footer included) and keep the text a failure needs.

use agentkit_toolpacks_coding::Outcome;
use agentkit_toolpacks_coding::shape::{
    DEFAULT_INLINE_LIMIT, ShapeInput, Shaped, shape_for_store, shape_outcome, shape_text,
};
use serde_json::{Value, json};

fn fixture(tool: &str, name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/shape/{tool}/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn shape(tool: &str, args: &Value, text: &str) -> Option<Shaped> {
    let input = ShapeInput {
        tool,
        args,
        text,
        ok: true,
        inline_limit: DEFAULT_INLINE_LIMIT,
    };
    shape_text(&input).expect("a rule does not fail on a fixture")
}

/// The 20% bar: the shaped text and its footer are at most 80% of the raw.
fn assert_saves_a_fifth(raw: &str, shaped: &Shaped) {
    assert!(
        shaped.cost() * 100 <= raw.len() * 80,
        "{} bytes shaped (with footer) from {} raw",
        shaped.cost(),
        raw.len()
    );
    assert!(shaped.hidden > 0);
}

fn shaped_fixture(tool: &str, name: &str, args: &Value) -> (String, Shaped) {
    let raw = fixture(tool, name);
    let shaped = shape(tool, args, &raw).unwrap_or_else(|| panic!("{tool}/{name} was not shaped"));
    assert_saves_a_fifth(&raw, &shaped);
    (raw, shaped)
}

fn kept(shaped: &Shaped, needles: &[&str]) {
    for needle in needles {
        assert!(
            shaped.text.contains(needle),
            "lost: {needle}\n{}",
            shaped.text
        );
    }
}

fn dropped(shaped: &Shaped, needles: &[&str]) {
    for needle in needles {
        assert!(!shaped.text.contains(needle), "kept: {needle}");
    }
}

// ---- run_tests

#[test]
fn cargo_test_keeps_the_failures_and_counts_the_rest() {
    let (_, shaped) = shaped_fixture(
        "run_tests",
        "cargo_fail.txt",
        &json!({"command": "cargo test"}),
    );
    kept(
        &shaped,
        &[
            "cargo test: 40 passed, 2 failed, 2 ignored",
            "test store::outputs::expiry_moves_forward ... FAILED",
            "---- store::outputs::expiry_moves_forward stdout ----",
            "assertion `left == right` failed: a read must move the expiry of every output of the session",
            "  left: 1763251200000",
            " right: 1765843200000",
            "called `Result::unwrap()` on an `Err` value: CoreError",
            "test result: FAILED. 40 passed; 2 failed; 2 ignored",
            "[exit code 101]",
        ],
    );
    dropped(
        &shaped,
        &[
            "store::session::opens_in_memory ... ok",
            "Compiling agentkit-core",
        ],
    );
    // 40 passed and 2 ignored tests are passing-test lines; the other six
    // dropped lines are `running N tests` and build chatter.
    assert_eq!(shaped.what, "lines (42 passing-test, 6 build/progress)");
}

#[test]
fn pytest_keeps_the_failures_and_the_summary() {
    let (_, shaped) = shaped_fixture(
        "run_tests",
        "pytest_fail.txt",
        &json!({"command": "pytest"}),
    );
    kept(
        &shaped,
        &[
            "pytest: 2 failed, 116 passed in 4.62s",
            "E       assert 16.0 == 8.0",
            "E       AssertionError: assert 'v' is None",
            "FAILED tests/test_retry.py::test_backoff_caps_at_max_delay - assert 16.0 == 8.0",
            "tests/test_cache.py:77: AssertionError",
            // A progress line with an F mark is a failure line and stays.
            "tests/test_retry.py ......F............",
            "tests/test_cache.py ...........F........",
            "[exit code 1]",
        ],
    );
    dropped(
        &shaped,
        &["platform darwin", "[ 11%]", "collected 118 items"],
    );
}

#[test]
fn jest_keeps_the_failing_block() {
    let (_, shaped) = shaped_fixture(
        "run_tests",
        "jest_fail.txt",
        &json!({"command": "npx jest"}),
    );
    kept(
        &shaped,
        &[
            "jest: 1 failed, 25 passed, 26 total",
            "✕ wraps long arrays",
            "● printer › wraps long arrays",
            "expect(received).toBe(expected) // Object.is equality",
            "at Object.<anonymous> (src/format/printer.test.ts:46:17)",
            "FAIL src/format/printer.test.ts",
        ],
    );
    dropped(
        &shaped,
        &["✓ splits identifiers", "PASS src/parser/tokenizer.test.ts"],
    );
}

#[test]
fn vitest_keeps_the_failing_assertion() {
    let (_, shaped) = shaped_fixture(
        "run_tests",
        "vitest_fail.txt",
        &json!({"command": "bunx vitest run"}),
    );
    kept(
        &shaped,
        &[
            "vitest: 1 failed | 17 passed (18)",
            "× cart > applies coupon",
            "AssertionError: expected 8999 to be 8099 // Object.is equality",
            "FAIL  src/store/cart.test.ts > cart > applies coupon",
            "Test Files  1 failed | 2 passed (3)",
        ],
    );
    dropped(&shaped, &["✓ date > formats iso"]);
}

#[test]
fn go_test_keeps_the_failing_test_and_its_log() {
    let (_, shaped) = shaped_fixture(
        "run_tests",
        "go_fail.txt",
        &json!({"command": "go test ./..."}),
    );
    kept(
        &shaped,
        &[
            "go test: 16 passed, 1 failed, 1 packages ok",
            "server_test.go:204: Content-Range = \"bytes 0-99/200\", want \"bytes 100-199/200\"",
            "--- FAIL: TestRangeRequest (0.00s)",
            "FAIL\texample.com/svc/http\t0.412s",
        ],
    );
    dropped(
        &shaped,
        &["=== RUN   TestParseBody", "--- PASS: TestParseBody"],
    );
}

#[test]
fn an_unknown_runner_keeps_its_text() {
    let raw = fixture("run_tests", "unknown_runner.txt");
    assert_eq!(shape("run_tests", &json!({"command": "rspec"}), &raw), None);
}

// ---- run_command

#[test]
fn run_command_collapses_build_progress_and_keeps_the_error() {
    let (_, shaped) = shaped_fixture(
        "run_command",
        "cargo_build_error.txt",
        &json!({"command": "cargo build"}),
    );
    kept(
        &shaped,
        &[
            "[… 12 progress lines]",
            "[… 20 progress lines]",
            // A crate name with `error` in it is kept: a line that names an
            // error is never dropped.
            "Compiling thiserror v1.0.61",
            "warning: unused variable: `limit`",
            "error[E0308]: mismatched types",
            "expected `OutputHandle`, found `String`",
            "error: could not compile `agentkit-store` (lib) due to 1 previous error",
            "[exit code 101]",
        ],
    );
    dropped(
        &shaped,
        &["Downloaded serde v1.0.203", "Compiling regex v1.10.5"],
    );
}

#[test]
fn run_command_collapses_package_manager_chatter() {
    let (_, shaped) = shaped_fixture(
        "run_command",
        "npm_install.txt",
        &json!({"command": "npm install"}),
    );
    kept(
        &shaped,
        &[
            "npm warn deprecated inflight@1.0.6",
            "added 312 packages, and audited 313 packages in 14s",
            "2 moderate severity vulnerabilities",
            "[exit code 0]",
        ],
    );
    dropped(
        &shaped,
        &["npm http fetch GET 200", "npm timing reify:unpack"],
    );
}

#[test]
fn run_command_collapses_image_pull_layers_and_keeps_the_build_error() {
    let (_, shaped) = shaped_fixture(
        "run_command",
        "docker_build_error.txt",
        &json!({"command": "docker build ."}),
    );
    kept(
        &shaped,
        &[
            "#5 ERROR: process",
            "0.412 E: Failed to fetch http://deb.debian.org/debian/pool/main/g/gcc-12/gcc-12_12.2.0-14_amd64.deb  Connection timed out",
            "ERROR: failed to solve",
            "[exit code 1]",
        ],
    );
    dropped(
        &shaped,
        &["Pulling fs layer", "Pull complete", "Verifying Checksum"],
    );
}

#[test]
fn run_command_without_progress_keeps_its_text() {
    let raw = fixture("run_command", "plain_output.txt");
    assert_eq!(
        shape("run_command", &json!({"command": "ls -l"}), &raw),
        None
    );
}

#[test]
fn a_buildkit_error_line_survives_the_progress_collapse() {
    let mut raw = String::new();
    for layer in 0..30 {
        raw.push_str(&format!(
            "#{layer} sha256:9f2c{layer:04x}e1 12.3MB / 45.6MB 2.1s\n"
        ));
        raw.push_str(&format!("#{layer} 13.1MB / 45.6MB 2.3s\n"));
    }
    raw.push_str("#31 ERROR: failed to compute cache key: \"/missing\" not found\n");
    raw.push_str("#32 [internal] load build context: Exception while reading context\n");
    raw.push_str("[exit code 1]");
    let shaped = shape("run_command", &json!({"command": "docker build ."}), &raw)
        .expect("progress collapses");
    kept(
        &shaped,
        &[
            "[… 60 progress lines]",
            "#31 ERROR: failed to compute cache key",
            "Exception while reading context",
            "[exit code 1]",
        ],
    );
    dropped(&shaped, &["sha256:"]);
}

#[test]
fn a_bare_percentage_line_is_not_progress() {
    let mut raw = String::new();
    for index in 0..40 {
        raw.push_str(&format!("   Compiling crate_{index} v0.1.0\n"));
    }
    raw.push_str("85% coverage threshold not met\n");
    raw.push_str("45%|████▌     | 9/20 [00:01<00:02]\n");
    raw.push_str("[exit code 1]");
    let shaped =
        shape("run_command", &json!({"command": "make check"}), &raw).expect("progress collapses");
    kept(
        &shaped,
        &["85% coverage threshold not met", "[exit code 1]"],
    );
    dropped(&shaped, &["9/20"]);
}

// ---- ANSI

#[test]
fn colored_cargo_output_is_shaped_and_its_failures_kept() {
    let raw = fixture("run_tests", "cargo_fail.txt")
        .replace(" ... ok", " ... \x1b[32mok\x1b[0m")
        .replace(" ... FAILED", " ... \x1b[31mFAILED\x1b[0m")
        .replace(
            "test result: FAILED.",
            "test result: \x1b[31mFAILED\x1b[0m.",
        )
        .replace("assertion `left", "\x1b[1massertion `left");
    let shaped = shape("run_tests", &json!({"command": "cargo test"}), &raw)
        .expect("a coloured run is shaped");
    kept(
        &shaped,
        &[
            "cargo test: 40 passed, 2 failed, 2 ignored",
            "test store::outputs::expiry_moves_forward ... FAILED",
            "assertion `left == right` failed",
        ],
    );
    dropped(&shaped, &["opens_in_memory", "\x1b["]);
}

#[test]
fn colored_pytest_output_keeps_its_summary_and_failure_marks() {
    let raw = fixture("run_tests", "pytest_fail.txt")
        .replace("......F............", "......\x1b[31mF\x1b[0m............")
        .replace(
            "=== 2 failed, 116 passed in 4.62s ===",
            "\x1b[31m=== 2 failed, 116 passed in 4.62s ===\x1b[0m",
        );
    let shaped =
        shape("run_tests", &json!({"command": "pytest"}), &raw).expect("a coloured run is shaped");
    kept(
        &shaped,
        &[
            "pytest: 2 failed, 116 passed in 4.62s",
            "tests/test_retry.py ......F............",
        ],
    );
}

// ---- code_search

#[test]
fn code_search_groups_by_file_with_caps_and_counts() {
    let args = json!({"pattern": "handle"});
    let (raw, shaped) = shaped_fixture("code_search", "broad.txt", &args);
    kept(
        &shaped,
        &[
            "77 result lines in 9 files",
            "crates/store/src/output_store.rs (22)",
            "… 16 more in this file",
            "[stopped: match limit 80 reached",
            "[more matches: narrow the search with path or glob, or raise limit]",
        ],
    );
    // 44 of 77 lines are shown: six per file, fewer for small files.
    assert_eq!(shaped.hidden, 33);
    assert_eq!(shaped.what, "matches");
    // A path is written once per file, not once per line.
    assert_eq!(
        shaped
            .text
            .matches("crates/store/src/output_store.rs")
            .count(),
        1
    );
    assert!(raw.matches("crates/store/src/output_store.rs").count() > 20);
}

#[test]
fn code_search_context_lines_are_capped_and_counted() {
    let mut raw = String::new();
    for file in 0..3 {
        for line in 0..30 {
            raw.push_str(&format!(
                "src/mod_{file}.rs:{}:    handle(call_{line});\n",
                line * 3 + 1
            ));
        }
    }
    for line in 0..50 {
        raw.push_str(&format!(
            "src/mod_0.rs-{}-    // context line {line}\n",
            line + 100
        ));
    }
    raw.push_str("[stopped: match limit 90 reached]");
    let shaped = shape("code_search", &json!({"pattern": "handle"}), &raw).expect("shaped");
    kept(
        &shaped,
        &[
            "context lines (50):",
            "// context line 19",
            "[stopped: match limit 90 reached]",
        ],
    );
    dropped(&shaped, &["context line 20"]);
    // 90 hits show 18 of them (six per file): 72 hidden, plus 30 context lines.
    assert_eq!(shaped.hidden, 72 + 30);
}

#[test]
fn a_short_search_keeps_its_text() {
    let raw = fixture("code_search", "few.txt");
    assert_eq!(shape("code_search", &json!({"pattern": "fn"}), &raw), None);
}

// ---- read_file

fn whole(path: &str) -> Value {
    json!({ "file_path": path })
}

#[test]
fn a_large_rust_file_becomes_an_outline_and_its_first_lines() {
    let (_, shaped) = shaped_fixture(
        "read_file",
        "output_store.rs.txt",
        &whole("src/output_store.rs"),
    );
    kept(
        &shaped,
        &[
            "outline (line  symbol):",
            "pub struct SqliteOutputStore",
            "impl SqliteOutputStore",
            "pub async fn put(&self, session: &str, handle: &OutputHandle) -> Result<(), OutputError>",
            "mod tests",
            "first 40 lines:",
            "1\t//! Output store adapter",
        ],
    );
    assert_eq!(shaped.what, "lines");
}

#[test]
fn a_large_typescript_file_becomes_an_outline() {
    let (_, shaped) = shaped_fixture("read_file", "client.ts.txt", &whole("src/client.ts"));
    kept(
        &shaped,
        &[
            "export interface ClientOptions",
            "export type Handler",
            "export class ApiClient",
            "async getUser(id: string",
            "export const buildHeaders",
            "export function loadFixture",
        ],
    );
}

#[test]
fn a_large_python_file_becomes_an_outline() {
    let (_, shaped) = shaped_fixture("read_file", "loader.py.txt", &whole("loader.py"));
    kept(
        &shaped,
        &[
            "class Layer:",
            "def from_env(source: str) -> Layer:",
            "class Loader:",
            "async def refresh(self) -> None:",
        ],
    );
}

#[test]
fn a_large_go_file_becomes_an_outline() {
    let (_, shaped) = shaped_fixture("read_file", "server.go.txt", &whole("http/server.go"));
    kept(
        &shaped,
        &[
            "type Server struct",
            "func NewServer(timeout time.Duration) *Server",
            "func (s *Server) Route(pattern string, h http.HandlerFunc)",
            "const MaxBody = 1 << 20",
        ],
    );
}

#[test]
fn a_range_read_and_a_short_or_unknown_file_are_unchanged() {
    let raw = fixture("read_file", "output_store.rs.txt");
    assert_eq!(
        shape(
            "read_file",
            &json!({"file_path": "a.rs", "offset": 1, "limit": 400}),
            &raw
        ),
        None
    );
    assert_eq!(
        shape(
            "read_file",
            &json!({"file_path": "a.rs", "offset": 120}),
            &raw
        ),
        None
    );
    assert_eq!(
        shape("read_file", &json!({"paths": ["a.rs", "b.rs"]}), &raw),
        None
    );
    let short = fixture("read_file", "short.rs.txt");
    assert_eq!(shape("read_file", &whole("main.rs"), &short), None);
    let prose = fixture("read_file", "notes.txt.txt");
    assert_eq!(shape("read_file", &whole("notes.txt"), &prose), None);
}

// ---- edit_file and write_file

#[test]
fn a_large_edit_diff_is_bounded_and_keeps_its_header() {
    let (raw, shaped) = shaped_fixture(
        "edit_file",
        "large_edit.txt",
        &json!({"path": "crates/core/src/pipeline.rs"}),
    );
    kept(
        &shaped,
        &[
            "Edited crates/core/src/pipeline.rs (1 edit, +118 -110)",
            "sha256: 9c1f0e3a77b2d4e85a6f1c20b3d94e7f5a8c0d12e3f4a5b6c7d8e9f0a1b2c3d4",
            "--- a/crates/core/src/pipeline.rs",
            "@@ -40,110 +40,118 @@",
            "-    let step_1 = compute_1(&input, 1);",
            "[… 175 more diff lines; the edit was applied in full]",
        ],
    );
    assert_eq!(shaped.hidden, 175);
    dropped(&shaped, &["stage_118"]);
    assert!(raw.contains("stage_118"));
}

#[test]
fn a_small_edit_and_a_write_receipt_are_unchanged() {
    let small = fixture("edit_file", "small_edit.txt");
    assert_eq!(shape("edit_file", &json!({}), &small), None);
    let receipt = fixture("edit_file", "write_receipt.txt");
    assert_eq!(shape("write_file", &json!({}), &receipt), None);
}

// ---- git_inspect

#[test]
fn git_status_groups_by_code_and_collapses_crowded_groups() {
    let (_, shaped) = shaped_fixture("git_inspect", "status.txt", &json!({"command": "status"}));
    kept(
        &shaped,
        &[
            "## feat/output-store...origin/feat/output-store [ahead 3]",
            ".M (18): crates/core/src/ x18",
            "M. (5): crates/store/src/migrations/v15.sql",
            "D. (1): crates/core/src/envelope_old.rs",
            "?? (46): docs/notes/ x6, target-fixtures/generated/ x40",
        ],
    );
    assert_eq!(shaped.hidden, 64);
    assert_eq!(shaped.what, "paths");
}

#[test]
fn git_log_shows_the_newest_commits_and_writes_the_year_once() {
    let (_, shaped) = shaped_fixture(
        "git_inspect",
        "log.txt",
        &json!({"command": "log", "limit": 48}),
    );
    kept(&shaped, &["dates in 2026", "… 18 older commits"]);
    assert_eq!(shaped.hidden, 18);
    assert_eq!(shaped.text.lines().count(), 32);
    dropped(&shaped, &["2026-11-"]);
}

#[test]
fn git_diff_cuts_a_huge_file_diff_and_keeps_a_small_one_whole() {
    let (_, shaped) = shaped_fixture("git_inspect", "diff.txt", &json!({"command": "diff"}));
    kept(
        &shaped,
        &[
            "diff --git a/Cargo.lock b/Cargo.lock",
            "@@ -100,6 +100,260 @@ dependencies = [",
            "[… 201 more diff lines of Cargo.lock]",
            "diff --git a/crates/core/src/output.rs b/crates/core/src/output.rs",
            "-const LIMIT: usize = 8 * 1024;",
            "+const LIMIT: usize = 16 * 1024;",
            "+fn extra() {}",
        ],
    );
    // Two `index` lines and the 201 cut lines.
    assert_eq!(shaped.hidden, 203);
    dropped(&shaped, &["index 3f2a1b0..9d8c7e6", "dependency-crate-259"]);
}

#[test]
fn a_failed_git_call_is_not_shaped() {
    let args = json!({"command": "log"});
    let input = ShapeInput {
        tool: "git_inspect",
        args: &args,
        text: "git_failed: not a git repository",
        ok: false,
        inline_limit: DEFAULT_INLINE_LIMIT,
    };
    assert_eq!(shape_text(&input), Ok(None));
}

// ---- list_dir and find_files

#[test]
fn a_big_directory_becomes_counts_by_extension() {
    let (_, shaped) = shaped_fixture("list_dir", "big_dir.txt", &json!({"path": "."}));
    kept(
        &shaped,
        &[
            "244 entries: 7 dirs, 236 files, 1 links",
            "dirs: src/, tests/, docs/, benches/, examples/, scripts/, .github/",
            "links: current@",
            ".rs (141): ",
            ".json (60): ",
            "[12 ignored entries hidden]",
        ],
    );
    // 137 + 56 + 26 file names beyond the four shown per group.
    assert_eq!(shaped.hidden, 219);
    assert!(shaped.note.is_none());
}

#[test]
fn a_capped_listing_says_so_and_a_bracketed_file_is_an_entry() {
    let mut raw = String::from("[odd].txt\n(paren).md\n");
    for index in 0..498 {
        raw.push_str(&format!("file_{index:03}.rs\n"));
    }
    raw.push_str("[37 more entries not shown]");
    let shaped = shape("list_dir", &json!({}), &raw).expect("shaped");
    kept(
        &shaped,
        &[
            "500 entries shown (listing capped at 500): 0 dirs, 500 files, 0 links",
            "[37 more entries not shown]",
        ],
    );
    assert!(
        shaped
            .note
            .as_deref()
            .is_some_and(|note| note.contains("capped"))
    );
}

#[test]
fn a_big_find_result_becomes_a_tree_with_counts() {
    let (_, shaped) = shaped_fixture(
        "find_files",
        "find_files_many.txt",
        &json!({"pattern": "**/*.rs"}),
    );
    kept(
        &shaped,
        &[
            "198 files in 6 dirs",
            "crates/toolpacks-coding/src/ (45): file_00.rs, file_01.rs, file_02.rs, file_03.rs, … +41",
            "[stopped: result limit reached]",
        ],
    );
    assert_eq!(shaped.hidden, 174);
}

#[test]
fn a_capped_read_counts_from_the_file_length() {
    let mut raw = String::new();
    for line in 1..=2000 {
        raw.push_str(&format!("{line}\tfn function_{line}() {{}}\n"));
    }
    raw.push_str("[Showing lines 1-2000 of 5120 (50.0KB limit). Use offset=2001 to continue.]");
    let shaped = shape("read_file", &whole("src/big.rs"), &raw).expect("shaped");
    kept(
        &shaped,
        &["src/big.rs: 5,120 lines.", "[Showing lines 1-2000 of 5120"],
    );
    assert_eq!(shaped.hidden, 5120 - 40);
    let note = shaped.note.expect("note");
    assert!(
        note.contains("5,120 lines") && note.contains("lines 1-2000"),
        "{note}"
    );
}

#[test]
fn a_short_listing_is_unchanged() {
    let raw = fixture("list_dir", "small_dir.txt");
    assert_eq!(shape("list_dir", &json!({}), &raw), None);
    assert_eq!(shape("find_files", &json!({}), &raw), None);
}

// ---- guards

#[test]
fn a_teaching_failure_is_never_shaped() {
    let raw = fixture("run_tests", "cargo_fail.txt");
    let mut outcome = Outcome::ok(raw.clone());
    outcome.ok = false;
    outcome.details = Some(json!({"failure": {"code": "x"}}));
    let shaped = shape_outcome("run_tests", &json!({}), &mut outcome, DEFAULT_INLINE_LIMIT);
    assert!(shaped.is_none());
    assert!(outcome.warnings.is_empty());
    assert_eq!(outcome.text, raw);
}

#[test]
fn a_shaped_outcome_keeps_its_raw_text_for_the_store() {
    let raw = fixture("run_tests", "cargo_fail.txt");
    let mut outcome = Outcome::ok(raw.clone());
    let shaped = shape_outcome(
        "run_tests",
        &json!({"command": "cargo test"}),
        &mut outcome,
        DEFAULT_INLINE_LIMIT,
    );
    assert!(shaped.is_some());
    assert_eq!(outcome.text, raw);
    assert!(outcome.warnings.is_empty());
}

// ---- the store entry

fn failing_outcome(text: &str) -> Outcome {
    Outcome::ok(text.to_owned())
}

#[test]
fn a_result_within_the_inline_limit_is_not_shaped() {
    let raw = fixture("run_tests", "cargo_fail.txt");
    assert!(raw.len() < DEFAULT_INLINE_LIMIT);
    let mut outcome = failing_outcome(&raw);
    let args = json!({"command": "cargo test"});
    assert!(
        shape_for_store("run_tests", &args, &mut outcome, DEFAULT_INLINE_LIMIT, None).is_none()
    );
    // Over a tighter limit the same text is shaped, and the raw text is kept.
    let shaped = shape_for_store("run_tests", &args, &mut outcome, 1000, None).expect("shaped");
    assert_eq!(shaped.raw, raw);
}

#[test]
fn run_tests_counts_come_from_the_full_log() {
    let tail = fixture("run_tests", "cargo_fail.txt");
    let mut full = String::new();
    for index in 0..100 {
        full.push_str(&format!("test extra::case_{index} ... ok\n"));
    }
    full.push_str(&tail);
    let mut outcome = failing_outcome(&tail);
    outcome.truncated = true;
    let args = json!({"command": "cargo test"});
    let shape = shape_for_store("run_tests", &args, &mut outcome, 1000, Some(full.clone()))
        .expect("shaped");
    assert_eq!(shape.raw, full);
    assert!(
        shape
            .shaped
            .text
            .starts_with("cargo test: 140 passed, 2 failed, 2 ignored"),
        "{}",
        shape.shaped.text
    );
    // The full log is stored whole, so the tool's cap is not in the footer.
    assert!(shape.shaped.note.is_none());
}

#[test]
fn a_tool_that_capped_its_output_says_so_in_the_footer() {
    let raw = fixture("git_inspect", "status.txt");
    let mut outcome = failing_outcome(&raw);
    outcome.truncated = true;
    let args = json!({"command": "status"});
    let shape = shape_for_store("git_inspect", &args, &mut outcome, 1000, None).expect("shaped");
    let note = shape.shaped.note.expect("note");
    assert!(note.contains("tool output capped at 50 KB"), "{note}");
}
