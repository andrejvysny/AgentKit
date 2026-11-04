//! Integration against the REAL language servers. Skips with a loud
//! eprintln when a binary is absent; on the development host both exist
//! and these tests must pass.
#![cfg(unix)] // POSIX process tooling; the pool refuses non-unix hosts.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::CancellationToken;
use agentkit_lsp::{LspConfig, LspPool, ServerCommand, lsp_tools};
use serde_json::{Value, json};

mod support;
use support::{FixtureRoot, ToolHarness};

const RUST_ANALYZER: &str = "/usr/bin/rust-analyzer";

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates
        .and_then(|p| p.parent()) // rust
        .and_then(|p| p.parent()) // repo root
        .expect("workspace layout")
}

fn generous_config() -> LspConfig {
    LspConfig {
        request_timeout_ms: 60_000,
        init_timeout_ms: 60_000,
        idle_timeout_ms: 3_600_000,
        diagnostics_wait_ms: 3_000,
        ..LspConfig::default()
    }
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

/// Poll a tool call until the predicate accepts its payload (servers
/// answer `null` while indexing). Returns the last payload seen.
#[allow(unused_assignments)] // `last` is kept for the timeout report.
async fn poll_json(
    pool: &Arc<LspPool>,
    root: &Path,
    tool: &str,
    input: Value,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = Value::Null;
    loop {
        match call(pool, root, tool, input.clone()).await {
            Ok(data) => {
                if pred(&data) {
                    return data;
                }
                last = data;
            }
            Err(error) => {
                last = json!({"error": error.code, "message": error.message});
            }
        }
        if Instant::now() >= deadline {
            eprintln!("SKIP-FAIL: {what} never satisfied within 60s");
            return last;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll `probe` until it answers `true` or the deadline passes.
async fn poll_until<F, Fut>(what: &str, timeout: Duration, mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if probe().await {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("SKIP-FAIL: {what} did not settle within {timeout:?}");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_analyzer_full_suite() {
    if !Path::new(RUST_ANALYZER).exists() {
        eprintln!("SKIP: {RUST_ANALYZER} is not installed; skipping rust-analyzer suite");
        return;
    }
    let root = FixtureRoot::new("rust-project");
    let mut config = generous_config();
    config.servers.insert(
        "rust".into(),
        ServerCommand {
            command: RUST_ANALYZER.into(),
            args: vec![],
            env: BTreeMap::new(),
        },
    );
    let pool = LspPool::new(config);

    // definition: caller's call site lands on target's declaration.
    let data = poll_json(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "src/lib.rs", "line": 18, "character": 5}),
        "definition of target",
        |d| {
            d["definitions"]
                .as_array()
                .map(|items| {
                    items.iter().any(|x| {
                        x["file"] == "src/lib.rs" && x["line"] == 13 && x["character"] == 8
                    })
                })
                .unwrap_or(false)
        },
    )
    .await;
    let definitions = data["definitions"].as_array().unwrap();
    assert!(
        definitions
            .iter()
            .any(|d| { d["file"] == "src/lib.rs" && d["line"] == 13 && d["character"] == 8 }),
        "definition of target: {definitions:?}"
    );

    // references: caller site is reported.
    let data = poll_json(
        &pool,
        &root,
        "lsp_references",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        "references of target",
        |d| {
            d["references"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|r| r["file"] == "src/lib.rs" && r["line"] == 18)
                })
                .unwrap_or(false)
        },
    )
    .await;
    let references = data["references"].as_array().unwrap();
    assert!(
        references
            .iter()
            .any(|r| r["file"] == "src/lib.rs" && r["line"] == 18),
        "references must include caller: {references:?}"
    );

    // hover on target mentions target.
    let data = poll_json(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "src/lib.rs", "line": 13, "character": 8}),
        "hover on target",
        |d| d["contents"].as_str().is_some_and(|c| c.contains("target")),
    )
    .await;
    assert!(
        data["contents"].as_str().unwrap().contains("target"),
        "hover: {}",
        data["contents"]
    );

    // implementation: trait method declaration → impl location (line 8).
    let data = poll_json(
        &pool,
        &root,
        "lsp_implementation",
        json!({"file": "src/lib.rs", "line": 2, "character": 10}),
        "implementation of greet",
        |d| {
            d["implementations"]
                .as_array()
                .map(|items| items.iter().any(|i| i["line"] == 8))
                .unwrap_or(false)
        },
    )
    .await;
    let implementations = data["implementations"].as_array().unwrap();
    assert!(
        implementations.iter().any(|i| i["line"] == 8),
        "implementation of greet: {implementations:?}"
    );

    // workspace/symbol: indexing is slow — poll.
    let found = poll_until("workspace/symbol 'target'", Duration::from_secs(60), || {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        async move {
            let data = call(&pool, &root, "lsp_symbols", json!({"query": "target"}))
                .await
                .unwrap_or_default();
            data["symbols"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|s| s["name"] == "target" && s["file"] == "src/lib.rs")
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(found, "workspace symbol 'target' must be found");

    // diagnostics on the deliberately broken file: severity 1 error.
    // Each attempt rewrites the file so an unchanged-content poll still
    // triggers a fresh didChange → publishDiagnostics cycle.
    let bad = root.join("src/bad.rs");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let found = poll_until("diagnostics on bad.rs", Duration::from_secs(60), || {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        let bad = bad.clone();
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::write(
                &bad,
                format!("pub fn broken() -> u32 {{\n    \"not a number\" /* {n} */\n}}\n"),
            );
            let data = call(
                &pool,
                &root,
                "lsp_diagnostics",
                json!({"file": "src/bad.rs"}),
            )
            .await
            .unwrap_or_default();
            data["diagnostics"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|d| d["severity"] == 1 && d["file"] == "src/bad.rs")
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(found, "type error in bad.rs must surface as severity 1");

    // Shared pool: concurrent calls on the same root reuse ONE process.
    let (_, pid_before) = pool.slot_status(&root, "rust").expect("live server");
    let hovers = (0..4).map(|_| {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        tokio::spawn(async move {
            call(
                &pool,
                &root,
                "lsp_hover",
                json!({"file": "src/lib.rs", "line": 13, "character": 8}),
            )
            .await
        })
    });
    for handle in hovers {
        let outcome = handle.await.unwrap();
        assert!(outcome.is_ok(), "concurrent hover: {:?}", outcome.err());
    }
    let (_, pid_after) = pool.slot_status(&root, "rust").expect("live server");
    assert_eq!(
        pid_before, pid_after,
        "one shared process per root+language"
    );
    // rust-analyzer forks helper subprocesses, so assert by identity:
    // the pool's single parent process is the live server.
    assert!(support::process_alive(pid_after));

    pool.close().await;
    let deadline = Instant::now() + Duration::from_secs(15);
    while support::process_alive(pid_after) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !support::process_alive(pid_after),
        "close must terminate the server process"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typescript_language_server_full_suite() {
    let bin = repo_root().join("node_modules/.bin/typescript-language-server");
    if !bin.exists() {
        eprintln!("SKIP: {bin:?} is not installed; skipping typescript suite");
        return;
    }
    let root = FixtureRoot::new("ts-project");
    let mut config = generous_config();
    config.servers.insert(
        "typescript".into(),
        ServerCommand {
            command: bin.to_string_lossy().into_owned(),
            args: vec!["--stdio".into()],
            env: BTreeMap::new(),
        },
    );
    let pool = LspPool::new(config);

    // definition from main.ts call site → mod.ts declaration line 1.
    let data = poll_json(
        &pool,
        &root,
        "lsp_definition",
        json!({"file": "main.ts", "line": 3, "character": 31}),
        "definition of target (ts)",
        |d| {
            d["definitions"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|x| x["file"] == "mod.ts" && x["line"] == 1)
                })
                .unwrap_or(false)
        },
    )
    .await;
    let definitions = data["definitions"].as_array().unwrap();
    assert!(
        definitions
            .iter()
            .any(|d| d["file"] == "mod.ts" && d["line"] == 1),
        "definition of target: {definitions:?}"
    );

    // references of target: main.ts usage is reported.
    let data = poll_json(
        &pool,
        &root,
        "lsp_references",
        json!({"file": "mod.ts", "line": 1, "character": 17}),
        "references of target (ts)",
        |d| {
            d["references"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|r| r["file"] == "main.ts" && r["line"] == 3)
                })
                .unwrap_or(false)
        },
    )
    .await;
    let references = data["references"].as_array().unwrap();
    assert!(
        references
            .iter()
            .any(|r| r["file"] == "main.ts" && r["line"] == 3),
        "references must include main.ts: {references:?}"
    );

    // hover on target mentions target.
    let data = poll_json(
        &pool,
        &root,
        "lsp_hover",
        json!({"file": "mod.ts", "line": 1, "character": 17}),
        "hover on target (ts)",
        |d| d["contents"].as_str().is_some_and(|c| c.contains("target")),
    )
    .await;
    assert!(
        data["contents"].as_str().unwrap().contains("target"),
        "hover: {}",
        data["contents"]
    );

    // workspace/symbol finds target (tsserver warm-up — poll).
    let found = poll_until("workspace/symbol 'target'", Duration::from_secs(60), || {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        async move {
            let data = call(&pool, &root, "lsp_symbols", json!({"query": "target"}))
                .await
                .unwrap_or_default();
            data["symbols"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|s| s["name"] == "target" && s["file"] == "mod.ts")
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(found, "workspace symbol 'target' must be found");

    // diagnostics: the deliberate type error reports severity 1. Each
    // attempt rewrites the file so an unchanged-content poll still
    // triggers a fresh didChange → publishDiagnostics cycle.
    let err_file = root.join("err.ts");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let found = poll_until("diagnostics on err.ts", Duration::from_secs(60), || {
        let pool = Arc::clone(&pool);
        let root = root.path.clone();
        let err_file = err_file.clone();
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::write(
                &err_file,
                format!("export const wrong: number = \"definitely not a number\"; // {n}\n"),
            );
            let data = call(&pool, &root, "lsp_diagnostics", json!({"file": "err.ts"}))
                .await
                .unwrap_or_default();
            data["diagnostics"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .any(|d| d["severity"] == 1 && d["file"] == "err.ts")
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(found, "type error in err.ts must surface as severity 1");

    // Shared pool: one tsserver-backed process for the root.
    let (_, pid_before) = pool.slot_status(&root, "typescript").expect("live");
    for _ in 0..3 {
        call(
            &pool,
            &root,
            "lsp_hover",
            json!({"file": "mod.ts", "line": 1, "character": 17}),
        )
        .await
        .unwrap();
    }
    let (_, pid_after) = pool.slot_status(&root, "typescript").expect("live");
    assert_eq!(pid_before, pid_after);

    pool.close().await;
}
