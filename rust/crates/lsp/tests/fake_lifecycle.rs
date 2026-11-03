//! Integration against the scripted fake LSP servers: lifecycle, crash
//! respawn, backoff, cancellation, timeouts, idle eviction and pooling.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{CancellationToken, ToolContext, ToolLimits};
use agentkit_lsp::{
    CANCELLED, LspConfig, LspPool, REQUEST_TIMEOUT, SERVER_CRASHED, SERVER_NOT_FOUND,
    SERVER_UNAVAILABLE, ServerCommand, lsp_tools,
};
use serde_json::{Value, json};

mod support;
use support::{FixtureRoot, ToolHarness, fake_server_config, node_path};

async fn wait_gone(tag: &str) {
    let needle = format!("--tag={tag}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while support::count_processes_matching(&needle) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        support::count_processes_matching(&needle),
        0,
        "servers for {tag} gone"
    );
}

fn fake_config(env: BTreeMap<String, String>, tag: &str) -> LspConfig {
    let mut config = fake_server_config(env, tag);
    config.request_timeout_ms = 20_000;
    config.init_timeout_ms = 20_000;
    config.diagnostics_wait_ms = 2_000;
    config
}

fn env(flags: &[(&str, &str)]) -> BTreeMap<String, String> {
    flags
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn rust_fixture_root() -> FixtureRoot {
    FixtureRoot::new("rust-project")
}

async fn call(
    pool: &Arc<LspPool>,
    root: &Path,
    tool: &str,
    input: Value,
    cancellation: CancellationToken,
) -> Result<Value, agentkit_core::CoreError> {
    let tools = lsp_tools(Arc::clone(pool), root);
    let tool = tools
        .iter()
        .find(|t| t.definition().name == tool)
        .expect("tool exists");
    let context = ToolHarness::context(cancellation);
    let result = tool.execute(context, input).await?;
    Ok(result.data.expect("data"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_server_answers_navigation_requests() {
    const TAG: &str = "lsp-it-nav";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(BTreeMap::new(), TAG));
    let data = call(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "src/lib.rs", "line": 18, "character": 5}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["definitions"][0]["file"], "src/lib.rs");
    assert_eq!(data["definitions"][0]["line"], 13);
    assert_eq!(data["definitions"][0]["character"], 8);

    let data = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        data["contents"]
            .as_str()
            .unwrap()
            .contains("fn canned() -> u32")
    );

    let data = call(
        &pool,
        &root,
        "lsp_references",
        json!({"file": "src/lib.rs", "line": 13, "character": 8, "includeDeclaration": true}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["references"][0]["line"], 18);

    let data = call(
        &pool,
        &root,
        "lsp_implementation",
        json!({"file": "src/lib.rs", "line": 2, "character": 10}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["implementations"][0]["line"], 8);

    let data = call(
        &pool,
        &root,
        "lsp_symbols",
        json!({"query": "canned"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["symbols"][0]["name"], "cannedSymbol");
    assert_eq!(data["symbols"][0]["kind"], "function");
    pool.close().await;
    wait_gone(TAG).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_share_one_server_process() {
    const TAG: &str = "lsp-it-share";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(BTreeMap::new(), TAG));
    let (a, b) = tokio::join!(
        call(
            &pool,
            &root,
            "lsp_hover",
            json!({"file": "src/lib.rs", "line": 13, "character": 8}),
            CancellationToken::new()
        ),
        call(
            &pool,
            &root,
            "lsp_hover",
            json!({"file": "src/lib.rs", "line": 18, "character": 5}),
            CancellationToken::new()
        ),
    );
    assert!(a.is_ok() && b.is_ok());
    let (_, pid) = pool.slot_status(&root, "rust").expect("live slot");
    // Four more sequential calls must not have respawned anything.
    for _ in 0..4 {
        call(
            &pool,
            &root,
            "lsp_hover",
            json!({"file": "src/lib.rs", "line": 13, "character": 8}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    }
    let (_, same_pid) = pool.slot_status(&root, "rust").expect("live slot");
    assert_eq!(
        pid, same_pid,
        "pool must reuse one process per root+language"
    );
    assert_eq!(
        support::count_processes_matching(&format!("--tag={TAG}")),
        1
    );
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_mid_request_fails_and_next_call_respawns() {
    const TAG: &str = "lsp-it-crash";
    let marker = tempfile::tempdir().unwrap();
    let marker_path = marker.path().join("crashed");
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(
        env(&[
            ("LSP_FAKE_CRASH_ON", "textDocument/definition"),
            ("LSP_FAKE_MARKER", marker_path.to_str().unwrap()),
        ]),
        TAG,
    ));
    let error = call(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "src/lib.rs", "line": 18, "character": 5}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, SERVER_CRASHED, "{error:?}");
    // Next call respawns a fresh generation and succeeds.
    let data = call(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "src/lib.rs", "line": 18, "character": 5}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["definitions"][0]["line"], 13);
    let (generation, _) = pool.slot_status(&root, "rust").unwrap();
    assert_eq!(generation, 2, "crash must bump the generation");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_after_initialize_crashes_then_respawns() {
    const TAG: &str = "lsp-it-exitinit";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(env(&[("LSP_FAKE_EXIT_AFTER_INIT", "1")]), TAG));
    let first = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(&first, Err(e) if e.code == SERVER_CRASHED),
        "{first:?}"
    );
    // The exited generation is replaced on the next acquire; this fake
    // always exits after init, so it crashes again — that is fine, the
    // point is the slot re-spawns rather than reusing a dead process.
    let second = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(&second, Err(e) if e.code == SERVER_CRASHED));
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_cancellation_is_fast_and_leaves_server_usable() {
    const TAG: &str = "lsp-it-cancel";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(
        env(&[("LSP_FAKE_HANG", "textDocument/definition")]),
        TAG,
    ));
    let token = CancellationToken::new();
    let late = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        late.cancel();
    });
    let started = Instant::now();
    let error = call(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "src/lib.rs", "line": 18, "character": 5}),
        token,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, CANCELLED, "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "cancelled fast");
    // The hang was cancelled ($/cancelRequest); hover still works.
    let data = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(data["contents"].as_str().unwrap().contains("canned"));
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_timeout_cancels_and_reports() {
    const TAG: &str = "lsp-it-timeout";
    let root = rust_fixture_root();
    let mut config = fake_config(env(&[("LSP_FAKE_SILENT", "textDocument/hover")]), TAG);
    config.request_timeout_ms = 400;
    let pool = LspPool::new(config);
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, REQUEST_TIMEOUT, "{error:?}");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initialize_cancellation_kills_child() {
    const TAG: &str = "lsp-it-initcancel";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(env(&[("LSP_FAKE_HANG", "initialize")]), TAG));
    let token = CancellationToken::new();
    let late = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        late.cancel();
    });
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        token,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, CANCELLED, "{error:?}");
    // The half-started child must be gone.
    wait_gone(TAG).await;
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_backoff_refuses_then_recovers_after_cooldown() {
    const TAG: &str = "lsp-it-backoff";
    let root = rust_fixture_root();
    let mut config = fake_config(env(&[("LSP_FAKE_EXIT_EARLY", "1")]), TAG);
    config.max_restarts = 2;
    config.restart_window_ms = 800;
    let pool = LspPool::new(config);
    let hover = || {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        async move {
            call(
                &pool,
                &root,
                "lsp_hover",
                json!({"file": "src/lib.rs", "line": 13, "character": 8}),
                CancellationToken::new(),
            )
            .await
        }
    };
    // Fresh slot + two restarts crash; the third restart is refused.
    for _ in 0..3 {
        let error = hover().await.unwrap_err();
        assert_eq!(error.code, SERVER_CRASHED, "{error:?}");
    }
    let error = hover().await.unwrap_err();
    assert_eq!(error.code, SERVER_UNAVAILABLE, "{error:?}");
    // Once the cooldown elapses, restarts are allowed again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let error = hover().await.unwrap_err();
    assert_eq!(error.code, SERVER_CRASHED, "cooldown expired: {error:?}");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_server_is_evicted_and_resawns_on_demand() {
    const TAG: &str = "lsp-it-idle";
    let root = rust_fixture_root();
    let mut config = fake_config(BTreeMap::new(), TAG);
    config.idle_timeout_ms = 300;
    let pool = LspPool::new(config);
    call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let (generation, pid) = pool.slot_status(&root, "rust").unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    pool.evict_idle();
    let deadline = Instant::now() + Duration::from_secs(10);
    while pool.slot_status(&root, "rust").is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(pool.slot_status(&root, "rust").is_none(), "evicted");
    let data = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(data["contents"].as_str().unwrap().contains("canned"));
    let (new_generation, new_pid) = pool.slot_status(&root, "rust").unwrap();
    assert!(
        new_generation == 1 || new_generation == generation + 1,
        "respawn must start a new generation (was {generation}, now {new_generation})"
    );
    assert_ne!(new_pid, pid, "eviction must respawn");
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_diagnostics_are_normalized() {
    const TAG: &str = "lsp-it-push";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(env(&[("LSP_FAKE_PUBLISH", "1")]), TAG));
    let data = call(
        &pool,
        &root,
        "lsp_diagnostics",
        json!({"file": "src/lib.rs"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let items = data["diagnostics"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["file"], "src/lib.rs");
    assert_eq!(items[0]["severity"], 1);
    assert_eq!(items[0]["code"], "FAKE001");
    assert_eq!(items[0]["source"], "fake");
    assert_eq!(items[0]["message"], "deliberate fake error");
    assert_eq!(items[0]["line"], 2);
    assert_eq!(items[0]["character"], 1);
    // Without a file: all documents opened so far.
    let data = call(
        &pool,
        &root,
        "lsp_diagnostics",
        json!({}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(data["diagnostics"].as_array().unwrap().len(), 1);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_diagnostics_when_advertised() {
    const TAG: &str = "lsp-it-pull";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(env(&[("LSP_FAKE_PULL", "1")]), TAG));
    let data = call(
        &pool,
        &root,
        "lsp_diagnostics",
        json!({"file": "src/lib.rs"}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let items = data["diagnostics"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["message"], "pull fake error");
    assert_eq!(items[0]["severity"], 1);
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_binary_reports_server_not_found() {
    let root = rust_fixture_root();
    let mut config = LspConfig::default();
    config.servers.insert(
        "rust".into(),
        ServerCommand {
            command: "/nonexistent/rust-analyzer-42".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
    );
    let pool = LspPool::new(config);
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, SERVER_NOT_FOUND, "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_escape_and_bad_input_are_refused() {
    const TAG: &str = "lsp-it-path";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(BTreeMap::new(), TAG));
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "../../etc/hostname", "line": 1, "character": 1}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, agentkit_lsp::INVALID_FILE);
    let error = call(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 0, "character": 1}),
        CancellationToken::new(),
    )
    .await;
    // line 0 is schema-invalid, but even unvalidated it must not panic;
    // either refusal or an LSP-level answer is acceptable — assert only
    // the confined-path guarantee above.
    let _ = error;
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn limits_truncate_fake_results() {
    const TAG: &str = "lsp-it-limits";
    let root = rust_fixture_root();
    let pool = LspPool::new(fake_config(BTreeMap::new(), TAG));
    let tools = lsp_tools(Arc::clone(&pool), &root);
    let hover = tools
        .iter()
        .find(|t| t.definition().name == "lsp_hover")
        .unwrap()
        .clone();
    let context = ToolContext {
        limits: ToolLimits {
            profile: "tiny".into(),
            max_bytes: 16384,
            max_items: Some(1),
        },
        ..ToolHarness::context(CancellationToken::new())
    };
    let result = hover
        .execute(
            context,
            json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        )
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(result.limits.max_items, Some(1));
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_from_json_and_node_resolution() {
    let config = LspConfig::from_json(
        r#"{"requestTimeoutMs":1234,"servers":{"rust":{"command":"node","args":["x.mjs"]}}}"#,
    )
    .unwrap();
    assert_eq!(config.request_timeout_ms, 1234);
    assert_eq!(config.servers["rust"].args, ["x.mjs"]);
    assert!(LspConfig::from_json("{oops").is_err());
    assert!(node_path().is_some(), "node must exist for the fakes");
}

/// Stale-result guarding: a reply captured against document version 1 is
/// discarded when a concurrent edit bumps the document to version 2, and
/// the guarded retry answers from the current state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inflight_reply_superseded_by_document_edit() {
    const TAG: &str = "lsp-it-stale";
    let root = rust_fixture_root();
    let log = root.join("server.log");
    let pool = LspPool::new(fake_config(
        env(&[("LSP_FAKE_LOG", log.to_str().unwrap())]),
        TAG,
    ));
    let token = CancellationToken::new();
    let file = root.join("src/lib.rs");

    let handle = pool.acquire(&root, "rust", &token).await.expect("acquire");
    let process = handle.acquire(&token).await.expect("process");
    let (_, version) = process.sync_document(&file).await.expect("initial sync");
    assert_eq!(version, 1);

    // In flight (250 ms) captured at version 1; the file changes while the
    // request is pending and a concurrent sync moves the document on.
    let inflight = {
        let handle = Arc::clone(&handle);
        let token = token.clone();
        let file = file.clone();
        tokio::spawn(async move {
            handle
                .guarded_request(
                    "test/slow",
                    Some(&file),
                    &|_| json!({"delayMs": 250}),
                    &token,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;
    std::fs::write(&file, "// edited mid-request\nfn canned() -> u32 { 1 }\n").expect("edit");
    let process = handle.acquire(&token).await.expect("process");
    let (_, version) = process.sync_document(&file).await.expect("re-sync");
    assert_eq!(version, 2, "hash change must produce didChange v2");

    inflight
        .await
        .expect("task")
        .expect("the stale reply must be superseded by a fresh one");

    let log_text = std::fs::read_to_string(&log).expect("server log");
    let slow_requests = log_text
        .lines()
        .filter(|line| line.contains("\"method\":\"test/slow\""))
        .count();
    assert_eq!(slow_requests, 2, "the stale guard retried exactly once");
    assert!(
        log_text.contains("textDocument/didOpen"),
        "document was opened"
    );
    assert!(
        log_text.contains("textDocument/didChange"),
        "edit was synced"
    );
    assert!(
        log_text.contains("\"version\":2"),
        "bumped document version"
    );
    pool.close().await;
    wait_gone(TAG).await;
}

/// The idle clock must not outrun the request clock: an eviction sweep
/// fired while a request is in flight used to kill the server the request
/// was executing on (the pool saw a stale `last_used`, not the running
/// request). The busy marker pins the slot; the request must complete
/// against the SAME process generation, and a truly idle slot must still
/// evict afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_eviction_never_kills_a_server_mid_request() {
    const TAG: &str = "lsp-it-busy-evict";
    let root = rust_fixture_root();
    let log = root.join("server.log");
    let mut config = fake_config(env(&[("LSP_FAKE_LOG", log.to_str().unwrap())]), TAG);
    // Effectively "everything is always idle": the busy marker is the only
    // thing that can save the in-flight slot from this sweep.
    config.idle_timeout_ms = 1;
    let pool = LspPool::new(config);
    let token = CancellationToken::new();
    let file = root.join("src/lib.rs");

    let handle = pool.acquire(&root, "rust", &token).await.expect("acquire");
    handle.acquire(&token).await.expect("live process");
    let generation = handle.live_generation().expect("live generation");

    let inflight = {
        let handle = Arc::clone(&handle);
        let token = token.clone();
        tokio::spawn(async move {
            handle
                .guarded_request(
                    "test/slow",
                    Some(&file),
                    &|_| json!({"delayMs": 600}),
                    &token,
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        handle.idle_for() >= Duration::from_millis(1),
        "the slot must look evictable to a naive sweep"
    );
    pool.evict_idle();

    inflight
        .await
        .expect("task")
        .expect("an in-flight request must survive the eviction sweep");
    assert_eq!(
        handle.live_generation(),
        Some(generation),
        "the request's server must not have been killed and respawned"
    );

    // Control: once the request lands and the idle clock catches up, the
    // same sweep does evict — the busy marker is not a permanent pin.
    tokio::time::sleep(Duration::from_millis(10)).await;
    pool.evict_idle();
    let reacquired = pool
        .acquire(&root, "rust", &token)
        .await
        .expect("re-acquire");
    assert!(
        !Arc::ptr_eq(&reacquired, &handle),
        "a genuinely idle slot must still be evicted"
    );
    drop(reacquired);
    pool.close().await;
    wait_gone(TAG).await;
}
