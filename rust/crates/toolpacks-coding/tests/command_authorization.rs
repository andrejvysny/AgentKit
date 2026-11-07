//! End-to-end host authorization with inert tools; no shell commands execute.
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputStatus, STANDING_APPROVALS_METADATA_KEY, SqliteStore,
};
use agentkit_toolpacks_coding::command_policy::{
    CodingStandingApproval, ShellArgs, StandingApprovalWorkspace, WorkspaceResolver,
    coding_standing_approval_key,
};
use agentkit_toolpacks_coding::types::CodingMode;
use agentkit_toolpacks_coding::{
    CodingPermissionGuard, CodingPermissionGuardOptions, HighRiskCommandGuard,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Provider {
    command: String,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let call = agentkit_core::ToolCall {
            id: "call".into(),
            name: "run_command".into(),
            arguments_json: json!({"command":self.command}).to_string(),
        };
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "".into(),
            tool_calls: vec![call],
            finish_reason: "tool_calls".into(),
        })]))
    }
}

struct Inert {
    definition: ToolDefinition,
    hits: Arc<AtomicUsize>,
    /// Harmless counter file: the body appends one byte every time the host
    /// actually runs the command, so "zero execution" is provable on disk and
    /// not merely in this process's atomics.
    counter: Option<std::path::PathBuf>,
}
#[async_trait]
impl Tool for Inert {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = &self.counter {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("counter file is writable");
            file.write_all(b"1").expect("counter byte written");
        }
        Ok(ToolResult {
            ok: true,
            data: None,
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,

            model_content: None,
        })
    }
}
struct Contributor(Arc<dyn Tool>);
#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(&self, _: &ToolContributionContext) -> HostResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::DirectWrite(self.0.clone())])
    }
}
struct Resolver {
    root: String,
    environment: Option<HashMap<String, String>>,
}
#[async_trait]
impl WorkspaceResolver for Resolver {
    async fn resolve(&self, _: &str) -> Option<StandingApprovalWorkspace> {
        Some(StandingApprovalWorkspace {
            root: self.root.clone(),
            identity: "test".into(),
            context: None,
            environment: self.environment.clone(),
        })
    }
}

struct Fixture {
    store: SqliteStore,
    service: AgentSessionService,
    host: AgentHost,
    hits: Arc<AtomicUsize>,
}
async fn fixture(mode: CodingMode, command: &str, root: &str) -> Fixture {
    fixture_with_environment(mode, command, root, None).await
}
async fn fixture_with_environment(
    mode: CodingMode,
    command: &str,
    root: &str,
    environment: Option<HashMap<String, String>>,
) -> Fixture {
    fixture_inner(mode, command, root, environment, None).await
}
async fn fixture_inner(
    mode: CodingMode,
    command: &str,
    root: &str,
    environment: Option<HashMap<String, String>>,
    counter: Option<std::path::PathBuf>,
) -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let hits = Arc::new(AtomicUsize::new(0));
    let definition = serde_json::from_value(json!({"name":"run_command", "version":"1", "effect":"write", "capability":"process.exec", "description":"inert", "inputSchema":{"type":"object"}})).unwrap();
    let tool = Arc::new(Inert {
        definition,
        hits: hits.clone(),
        counter,
    });
    let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
        mode,
        role: None,
        authority: None,
        plan_tool_names: None,
    });
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Provider {
                command: command.into(),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(Contributor(tool))],
            tool_guards: vec![Arc::new(guard), Arc::new(HighRiskCommandGuard)],
            standing: Some(Arc::new(CodingStandingApproval {
                resolver: Resolver {
                    root: root.into(),
                    environment,
                },
            })),
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.into(), executor)]),
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    })
    .with_profiles(Arc::new(
        ProfileRegistry::new(&[json!({
            "id": "p",
            "kind": "coding",
            "instructions": "Exercise command authorization with inert tools only.",
            "tools": ["run_command"],
            "permissionMode": mode.as_str(),
            "limits": {"maxToolIterations": 4, "maxToolCallsPerIteration": 8},
            "resources": {"projectInstructions": false}
        })])
        .unwrap(),
    ));
    create_session(&service).await;
    Fixture {
        store,
        service,
        host,
        hits,
    }
}
async fn create_session(service: &AgentSessionService) {
    service
        .create_session(&CreateSessionInput {
            session_id: Some("chat".into()),
            title: None,
            profile_id: "p".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
}
async fn run(f: &Fixture) -> String {
    let sent = f
        .service
        .send(
            "chat",
            &SendInput {
                content: json!("test"),
                input_id: None,
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("expected turn")
    };
    f.host.run_once().await.unwrap();
    turn_id
}
async fn pending(f: &Fixture, turn: &str) -> Vec<agentkit_store::AgentInputRecord> {
    f.store
        .list_inputs(AgentInputQuery {
            session_id: "chat".into(),
            kinds: Some(vec![AgentInputKind::HumanRequest]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: Some(turn.into()),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn risky_commands_stop_before_inert_executor_in_every_mode() {
    let root = tempfile::tempdir().unwrap();
    for mode in [
        CodingMode::ReadOnly,
        CodingMode::Ask,
        CodingMode::AutoEdit,
        CodingMode::FullAuto,
        CodingMode::Plan,
    ] {
        for command in [
            "npx rimraf x",
            "git -c core.sshCommand=evil status",
            "\"/bin/rm\" inert",
            "node --eval=1",
        ] {
            let f = fixture(mode, command, root.path().to_str().unwrap()).await;
            let turn = run(&f).await;
            assert_eq!(f.hits.load(Ordering::SeqCst), 0, "{mode:?}: {command}");
            let requests = pending(&f, &turn).await;
            let approvable = matches!(
                mode,
                CodingMode::Ask | CodingMode::AutoEdit | CodingMode::FullAuto
            );
            assert_eq!(
                requests.len(),
                usize::from(approvable),
                "{mode:?}: {command}"
            );
            if let Some(request) = requests.first() {
                assert!(request.content.get("standing").is_none());
            }
            assert!(f.store.list_executions(turn).await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn old_unbound_standing_grant_never_authorizes_script() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = canonical.to_str().unwrap();
    // Hermetic identity: `npm` resolves to a stub, so the v2 key is issued
    // deterministically and cannot accidentally depend on the test host.
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("npm");
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let environment = HashMap::from([(
        "PATH".to_owned(),
        std::fs::canonicalize(&bin)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    )]);
    let f =
        fixture_with_environment(CodingMode::Ask, "npm run test", root, Some(environment)).await;
    // Former v1 keys contained no executable or environment fingerprint.
    let payload = json!({"v":1,"chatId":"chat","tool":"run_command","identity":"test","root":root,"cwd":root,"command":"npm run test","context":null});
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_string(&payload).unwrap().as_bytes())
    );
    let mut chat = f.store.get_chat("chat").await.unwrap().unwrap();
    chat.metadata[STANDING_APPROVALS_METADATA_KEY] = json!({"version":1,"grants":[{"grantId":"old","digest":digest,"label":"npm run test","toolName":"run_command","sourceRequestId":"old-request","answerId":"old-answer","grantedAt":"2026-01-01T00:00:00.000Z"}]});
    assert!(agentkit_store::find_active_standing_grant(&chat.metadata, &digest).is_some());
    f.store
        .update_chat("chat", None, Some(chat.metadata), None, 0)
        .await
        .unwrap();
    let turn = run(&f).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    let requests = pending(&f, &turn).await;
    assert_eq!(requests.len(), 1);
    // The pending request now carries a current-version (v4) key whose
    // digest differs from the legacy one — which is exactly why the old
    // grant must not settle it.
    let standing = requests[0]
        .content
        .get("standing")
        .expect("the current-version fingerprint key is offered");
    assert_ne!(standing["digest"].as_str().unwrap(), digest);
    assert_eq!(standing["label"].as_str().unwrap(), "npm run test");
    assert!(f.store.list_executions(turn).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Standing-approval fingerprint regressions (external review at 82ad438):
// approve -> mutate identity material -> re-run must take the ask path with
// ZERO execution, proven on a harmless counter file the tool body appends to
// only when the host actually ran the command.
// ---------------------------------------------------------------------------

/// A hermetic `bun test` workspace: `bun` and `node` resolve to stubs, and
/// package.json's `test` script selects `check.mjs` as the one directly-
/// selected startup input bound into the fingerprint (baseline
/// `bindScriptEntrypoints` -> `directScriptFiles("node check.mjs")`).
fn script_tree(dir: &std::path::Path) -> HashMap<String, String> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["bun", "node"] {
        let stub = bin.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"t","scripts":{"test":"node check.mjs"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("check.mjs"), "console.log(1)\n").unwrap();
    HashMap::from([(
        "PATH".to_owned(),
        std::fs::canonicalize(&bin)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    )])
}

async fn mint_key(root: &str, environment: &HashMap<String, String>) -> String {
    let workspace = StandingApprovalWorkspace {
        root: root.to_owned(),
        identity: "test".into(),
        context: None,
        environment: Some(environment.clone()),
    };
    coding_standing_approval_key(
        "coding",
        Some("chat"),
        "run_command",
        &ShellArgs {
            command: "bun test".into(),
            cwd: None,
        },
        &workspace,
    )
    .await
    .expect("the hermetic identity is keyable")
    .digest
}

async fn grant(f: &Fixture, digest: &str) {
    let mut chat = f.store.get_chat("chat").await.unwrap().unwrap();
    chat.metadata[STANDING_APPROVALS_METADATA_KEY] = json!({"version":1,"grants":[{"grantId":"g","digest":digest,"label":"bun test","toolName":"run_command","sourceRequestId":"r","answerId":"a","grantedAt":"2026-01-01T00:00:00.000Z"}]});
    f.store
        .update_chat("chat", None, Some(chat.metadata), None, 0)
        .await
        .unwrap();
}

fn counter_bytes(counter: &std::path::Path) -> u64 {
    std::fs::metadata(counter)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Findings 2+3 end to end: an approved `bun test` (whose startup input set
/// was selected from a clean environment) must NOT run once
/// `NODE_OPTIONS='--require ./evil.js'` enters the EFFECTIVE environment at
/// claim time — zero executions, a fresh ask instead. The counter file proves
/// the body never ran, and a control run under the approved environment
/// proves the counter is not a dead letter.
#[tokio::test]
async fn standing_approval_zero_execution_when_node_options_selects_evil_require() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = canonical.to_str().unwrap();
    let environment = script_tree(dir.path());
    std::fs::write(
        dir.path().join("evil.js"),
        "require('node:child_process')\n",
    )
    .unwrap();
    let counter = dir.path().join("counter.bin");

    let approved = mint_key(root, &environment).await;

    // Attack: the SAME command text, but the effective environment now asks
    // node to require an attacker-named file before anything else.
    let mut poisoned = environment.clone();
    poisoned.insert("NODE_OPTIONS".to_owned(), "--require ./evil.js".to_owned());
    let f = fixture_inner(
        CodingMode::Ask,
        "bun test",
        root,
        Some(poisoned),
        Some(counter.clone()),
    )
    .await;
    grant(&f, &approved).await;
    let turn = run(&f).await;
    assert_eq!(
        counter_bytes(&counter),
        0,
        "a standing approval minted without NODE_OPTIONS must not execute under it"
    );
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    let requests = pending(&f, &turn).await;
    assert_eq!(requests.len(), 1, "the mutated identity re-asks instead");
    assert_ne!(
        requests[0].content["standing"]["digest"].as_str().unwrap(),
        approved,
        "the new key binds the NODE_OPTIONS value"
    );
    assert!(f.store.list_executions(turn).await.unwrap().is_empty());

    // Control: under the approved environment the same grant does execute.
    let control = fixture_inner(
        CodingMode::Ask,
        "bun test",
        root,
        Some(environment.clone()),
        Some(counter.clone()),
    )
    .await;
    grant(&control, &approved).await;
    let turn = run(&control).await;
    assert_eq!(counter_bytes(&counter), 1, "the matching grant runs once");
    assert!(pending(&control, &turn).await.is_empty());
    assert!(
        !control
            .store
            .list_executions(turn)
            .await
            .unwrap()
            .is_empty(),
        "the admitted control execution left its receipt — the counter is not a dead letter"
    );
}

/// Findings 1+3 end to end: mutate ONE directly-selected startup input (the
/// script the package.json test entrypoint selects) after approval. The next
/// claim recomputes the fingerprint against the mutated file, the stored
/// grant no longer matches, and the counter proves zero execution after the
/// mutation — the first (pre-mutation) run under the grant does execute.
#[tokio::test]
async fn standing_approval_zero_execution_after_direct_startup_input_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = canonical.to_str().unwrap();
    let environment = script_tree(dir.path());
    let counter = dir.path().join("counter.bin");

    let approved = mint_key(root, &environment).await;
    let f = fixture_inner(
        CodingMode::Ask,
        "bun test",
        root,
        Some(environment.clone()),
        Some(counter.clone()),
    )
    .await;
    grant(&f, &approved).await;
    let first = run(&f).await;
    assert_eq!(counter_bytes(&counter), 1, "granted call executes once");
    assert!(pending(&f, &first).await.is_empty());

    // Mutate the single directly-selected startup input.
    std::fs::write(dir.path().join("check.mjs"), "console.log(2)\n").unwrap();
    let second = run(&f).await;
    assert_eq!(
        counter_bytes(&counter),
        1,
        "editing a bound startup input must not execute under the old approval"
    );
    let requests = pending(&f, &second).await;
    assert_eq!(requests.len(), 1, "the mutated input forces a fresh ask");
    assert_ne!(
        requests[0].content["standing"]["digest"].as_str().unwrap(),
        approved
    );
    assert!(f.store.list_executions(second).await.unwrap().is_empty());
}

/// Finding 4 end to end: an identity saved by the UNSAFE path — a digest
/// computed from the pre-fix key material under the old `v: 2` envelope —
/// never authorizes again. The grant row is shape-valid and active; only the
/// version inside the hashed envelope keeps it inert.
#[tokio::test]
async fn unsafe_saved_identity_never_authorizes_under_the_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = canonical.to_str().unwrap();
    let environment = script_tree(dir.path());
    let counter = dir.path().join("counter.bin");

    // Reproduce the pre-fix digest: the CURRENT fingerprint (the fingerprint
    // body itself was not re-versioned) wrapped in the legacy v2 envelope.
    let canonical_root = agentkit_toolpacks_coding::canonical_root(root)
        .await
        .unwrap();
    let confined = agentkit_toolpacks_coding::resolve_inside_root(&canonical_root, ".")
        .await
        .unwrap();
    let cwd = std::fs::canonicalize(&confined.path)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let fingerprint = agentkit_toolpacks_coding::command_fingerprint(
        &canonical_root,
        &cwd,
        "bun test",
        &environment,
    )
    .ok()
    .unwrap()
    .expect("keyable");
    let quote = |value: &str| json!(value).to_string();
    let envelope = |version: u32| {
        format!(
            "{{\"v\":{version},\"fingerprint\":{},\"chatId\":{},\"tool\":{},\"identity\":{},\"root\":{},\"cwd\":{},\"command\":{},\"context\":null}}",
            quote(&fingerprint),
            quote("chat"),
            quote("run_command"),
            quote("test"),
            quote(&canonical_root),
            quote(&cwd),
            quote("bun test"),
        )
    };
    let unsafe_digest = format!("{:x}", Sha256::digest(envelope(2).as_bytes()));
    let minted = mint_key(root, &environment).await;
    assert_ne!(
        unsafe_digest, minted,
        "the version bump structurally retires the unsafe identity"
    );

    let f = fixture_inner(
        CodingMode::Ask,
        "bun test",
        root,
        Some(environment.clone()),
        Some(counter.clone()),
    )
    .await;
    grant(&f, &unsafe_digest).await;
    let turn = run(&f).await;
    assert_eq!(
        counter_bytes(&counter),
        0,
        "an unsafe-version grant must never authorize execution"
    );
    let requests = pending(&f, &turn).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].content["standing"]["digest"].as_str().unwrap(),
        minted,
        "the fresh ask carries the current-version key"
    );
    assert!(f.store.list_executions(turn).await.unwrap().is_empty());
}

/// Hermetic P1-D workspace: `bun test` runs a package script that launches
/// `node cli.js` preloaded with `bootstrap.cjs` through the given selector
/// form (the script line is kept constant for the whole scenario).
fn selector_tree(dir: &std::path::Path, script: &str) -> HashMap<String, String> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["bun", "node"] {
        let stub = bin.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"t","scripts":{{"test":"{script}"}}}}"#),
    )
    .unwrap();
    std::fs::write(dir.join("cli.js"), "console.log('cli')\n").unwrap();
    std::fs::write(dir.join("bootstrap.cjs"), "module.exports = 'v1'\n").unwrap();
    HashMap::from([(
        "PATH".to_owned(),
        std::fs::canonicalize(&bin)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    )])
}

/// External review P1-D at decision level, all selector forms: approve once
/// (counter byte + execution receipt), then mutate ONLY the
/// `NODE_OPTIONS`-selected startup file with the command and the effective
/// environment strings constant. The next claim must request a NEW approval
/// with ZERO new counter bytes and ZERO new execution receipts; a freshly
/// re-minted grant then authorizes the mutated content again (the counter is
/// not a dead letter).
#[tokio::test]
async fn standing_approval_zero_execution_after_node_options_startup_file_mutation() {
    for (index, (label, script, inherited)) in [
        (
            "inherited",
            "node cli.js",
            Some("--require=./bootstrap.cjs"),
        ),
        (
            "assignment",
            "NODE_OPTIONS=--require=./bootstrap.cjs node cli.js",
            None,
        ),
        (
            "env wrapper",
            "NODE_OPTIONS=--require=./bootstrap.cjs env node cli.js",
            None,
        ),
        (
            "explicit CLI control",
            "node --require ./bootstrap.cjs cli.js",
            None,
        ),
    ]
    .iter()
    .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let root = canonical.to_str().unwrap();
        let mut environment = selector_tree(dir.path(), script);
        if let Some(value) = inherited {
            environment.insert("NODE_OPTIONS".to_owned(), (*value).to_owned());
        }
        let counter = dir.path().join(format!("counter-{index}.bin"));
        let approved = mint_key(root, &environment).await;
        let f = fixture_inner(
            CodingMode::Ask,
            "bun test",
            root,
            Some(environment.clone()),
            Some(counter.clone()),
        )
        .await;
        grant(&f, &approved).await;
        let first = run(&f).await;
        assert_eq!(
            counter_bytes(&counter),
            1,
            "{label}: the granted call executes once"
        );
        assert!(pending(&f, &first).await.is_empty());
        assert!(
            !f.store.list_executions(first).await.unwrap().is_empty(),
            "{label}: the admitted run left its receipt"
        );

        // Mutate ONLY the selected file; the selector string and the whole
        // effective environment stay byte-constant.
        std::fs::write(
            dir.path().join("bootstrap.cjs"),
            "module.exports = 'MUTATED'\n",
        )
        .unwrap();
        let second = run(&f).await;
        assert_eq!(
            counter_bytes(&counter),
            1,
            "{label}: editing the NODE_OPTIONS-selected startup file must not execute under the old approval"
        );
        let requests = pending(&f, &second).await;
        assert_eq!(
            requests.len(),
            1,
            "{label}: the mutated file forces a fresh ask"
        );
        assert_ne!(
            requests[0].content["standing"]["digest"].as_str().unwrap(),
            approved,
            "{label}: the new key binds the mutated file"
        );
        assert!(f.store.list_executions(second).await.unwrap().is_empty());

        // A fresh approval on the mutated content authorizes again (a fresh
        // fixture: the previous turn still parks on its unanswered request).
        let reapproved = mint_key(root, &environment).await;
        assert_ne!(reapproved, approved, "{label}: the re-minted key differs");
        let control = fixture_inner(
            CodingMode::Ask,
            "bun test",
            root,
            Some(environment.clone()),
            Some(counter.clone()),
        )
        .await;
        grant(&control, &reapproved).await;
        let third = run(&control).await;
        assert_eq!(
            counter_bytes(&counter),
            2,
            "{label}: the counter is not a dead letter"
        );
        assert!(pending(&control, &third).await.is_empty());
    }
}

/// An identity saved by the UNSAFE v3 path — pre-fix key material that did
/// not bind `NODE_OPTIONS`-selected startup files, hashed under the `v: 3`
/// envelope — never authorizes again under v4. Mirrors the v2 regression
/// pinned by `unsafe_saved_identity_never_authorizes_under_the_new_version`.
#[tokio::test]
async fn unsafe_v3_saved_identity_never_authorizes_under_v4() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = canonical.to_str().unwrap();
    let environment = selector_tree(
        dir.path(),
        "NODE_OPTIONS=--require=./bootstrap.cjs node cli.js",
    );
    let counter = dir.path().join("counter.bin");

    // Reproduce the pre-fix digest: the CURRENT fingerprint wrapped in the
    // legacy v3 envelope (the fingerprint body was not re-versioned).
    let canonical_root = agentkit_toolpacks_coding::canonical_root(root)
        .await
        .unwrap();
    let confined = agentkit_toolpacks_coding::resolve_inside_root(&canonical_root, ".")
        .await
        .unwrap();
    let cwd = std::fs::canonicalize(&confined.path)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let fingerprint = agentkit_toolpacks_coding::command_fingerprint(
        &canonical_root,
        &cwd,
        "bun test",
        &environment,
    )
    .ok()
    .unwrap()
    .expect("keyable");
    let quote = |value: &str| json!(value).to_string();
    let envelope = |version: u32| {
        format!(
            "{{\"v\":{version},\"fingerprint\":{},\"chatId\":{},\"tool\":{},\"identity\":{},\"root\":{},\"cwd\":{},\"command\":{},\"context\":null}}",
            quote(&fingerprint),
            quote("chat"),
            quote("run_command"),
            quote("test"),
            quote(&canonical_root),
            quote(&cwd),
            quote("bun test"),
        )
    };
    let unsafe_digest = format!("{:x}", Sha256::digest(envelope(3).as_bytes()));
    let minted = mint_key(root, &environment).await;
    assert_ne!(
        unsafe_digest, minted,
        "the v3-to-v4 bump structurally retires the unbound identity"
    );

    let f = fixture_inner(
        CodingMode::Ask,
        "bun test",
        root,
        Some(environment.clone()),
        Some(counter.clone()),
    )
    .await;
    grant(&f, &unsafe_digest).await;
    let turn = run(&f).await;
    assert_eq!(
        counter_bytes(&counter),
        0,
        "a v3-version grant must never authorize execution"
    );
    let requests = pending(&f, &turn).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].content["standing"]["digest"].as_str().unwrap(),
        minted,
        "the fresh ask carries the current-version (v4) key"
    );
    assert!(f.store.list_executions(turn).await.unwrap().is_empty());
}
