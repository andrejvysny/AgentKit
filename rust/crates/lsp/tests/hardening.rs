//! Regression tests for document-sync ordering, position encoding, input
//! validation and process-group teardown, against the scripted fake server.
#![cfg(unix)] // POSIX process tooling; the pool refuses non-unix hosts.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use agentkit_core::CancellationToken;
use agentkit_lsp::{INVALID_INPUT, LspConfig, LspPool, lsp_tools};
use serde_json::{Value, json};

mod support;
use support::{FixtureRoot, ToolHarness, fake_server_config};

fn config(flags: &[(&str, &str)], tag: &str) -> LspConfig {
    let env: BTreeMap<String, String> = flags
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut config = fake_server_config(env, tag);
    config.request_timeout_ms = 20_000;
    config.init_timeout_ms = 20_000;
    config
}

async fn call(
    pool: &Arc<LspPool>,
    root: &Path,
    tool: &str,
    input: Value,
) -> Result<Value, agentkit_core::CoreError> {
    let tools = lsp_tools(Arc::clone(pool), root);
    let tool = tools
        .iter()
        .find(|t| t.definition().name == tool)
        .expect("tool exists");
    let result = tool
        .execute(ToolHarness::context(CancellationToken::new()), input)
        .await?;
    Ok(result.data.expect("data"))
}

fn logged(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_never_overtake_did_open() {
    let root = FixtureRoot::new("rust-project");
    let log = root.join("server.log");
    let pool = LspPool::new(config(
        &[("LSP_FAKE_LOG", log.to_str().unwrap())],
        "lsp-hard-open",
    ));
    let mut calls = Vec::new();
    for _ in 0..8 {
        let (pool, root) = (Arc::clone(&pool), root.path.clone());
        calls.push(tokio::spawn(async move {
            call(
                &pool,
                &root,
                "lsp_hover",
                json!({"file": "src/lib.rs", "line": 1, "character": 1}),
            )
            .await
        }));
    }
    for task in calls {
        task.await.unwrap().expect("hover");
    }
    let methods: Vec<String> = logged(&log)
        .iter()
        .filter_map(|entry| entry["method"].as_str().map(str::to_owned))
        .collect();
    let open = methods
        .iter()
        .position(|m| m == "textDocument/didOpen")
        .expect("didOpen sent");
    let first_hover = methods
        .iter()
        .position(|m| m == "textDocument/hover")
        .expect("hover sent");
    assert!(
        open < first_hover,
        "didOpen must precede every request: {methods:?}"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|m| *m == "textDocument/didOpen")
            .count(),
        1
    );
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn positions_are_utf16_on_the_wire_and_scalar_for_the_model() {
    let root = FixtureRoot::new("rust-project");
    let log = root.join("server.log");
    // `b` is scalar index 11 (1-based 12) but UTF-16 offset 12: the emoji
    // is a surrogate pair.
    std::fs::write(root.join("src/emoji.rs"), "let s = \"a😀b\";\n").unwrap();
    let pool = LspPool::new(config(
        &[("LSP_FAKE_LOG", log.to_str().unwrap())],
        "lsp-hard-utf16",
    ));
    call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/emoji.rs", "line": 1, "character": 12}),
    )
    .await
    .expect("hover");
    let entries = logged(&log);
    let hover = entries
        .iter()
        .find(|e| e["method"] == "textDocument/hover")
        .expect("hover logged");
    assert_eq!(
        hover["params"]["position"],
        json!({"line": 0, "character": 12})
    );
    let init = entries
        .iter()
        .find(|e| e["method"] == "initialize")
        .expect("initialize logged");
    assert_eq!(
        init["params"]["capabilities"]["general"]["positionEncodings"],
        json!(["utf-16"])
    );

    // A line past the end of the document is the caller's error.
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/emoji.rs", "line": 50, "character": 1}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, INVALID_INPUT);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_integer_positions_are_invalid_input() {
    let root = FixtureRoot::new("rust-project");
    let pool = LspPool::new(config(&[], "lsp-hard-int"));
    for (line, character) in [
        (json!(0), json!(1)),
        (json!(1.5), json!(1)),
        (json!("2"), json!(1)),
        (json!(1), json!(null)),
        (json!(-1), json!(1)),
    ] {
        let error = call(
            &pool,
            &root,
            "lsp_definition",
            json!({"file": "src/lib.rs", "line": line, "character": character}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, INVALID_INPUT, "{line} {character}");
    }
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_server_takes_its_helper_processes_with_it() {
    let root = FixtureRoot::new("rust-project");
    let pidfile = root.join("helper.pid");
    let pool = LspPool::new(config(
        &[("LSP_FAKE_GRANDCHILD", pidfile.to_str().unwrap())],
        "lsp-hard-group",
    ));
    call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 1, "character": 1}),
    )
    .await
    .expect("hover");
    let helper: u32 = std::fs::read_to_string(&pidfile)
        .expect("helper pid written")
        .trim()
        .parse()
        .unwrap();
    assert!(support::process_alive(helper));
    pool.close().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while support::process_alive(helper) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !support::process_alive(helper),
        "helper outlived the server"
    );
}
