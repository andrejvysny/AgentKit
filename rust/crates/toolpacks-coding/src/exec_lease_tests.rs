use super::*;
use crate::coordination::{CoordinationWait, CoordinatorObserver, InProcessCoordinator};
use crate::limits::CodingLimits;
use agentkit_core::{CoreError, DispatchFence, DispatchGuard, ToolLimits};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

struct Waiting(Notify);

impl CoordinatorObserver for Waiting {
    fn on_waiting(&self, _: &CoordinationWait) {
        self.0.notify_one();
    }
}

#[derive(Debug)]
struct Revocable(AtomicBool);

#[async_trait::async_trait]
impl DispatchGuard for Revocable {
    async fn check(&self) -> Result<(), CoreError> {
        if self.0.load(Ordering::SeqCst) {
            Err(CoreError::new("lease_lost", "test authority revoked"))
        } else {
            Ok(())
        }
    }

    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    env: Arc<PackEnv>,
    waiting: Arc<Waiting>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let waiting = Arc::new(Waiting(Notify::new()));
        let coordinator = Arc::new(InProcessCoordinator::new(Some(waiting.clone())));
        let env = Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: root.to_string_lossy().into_owned(),
            limits: CodingLimits::default(),
            allow_sensitive: false,
            clock: crate::types::system_clock(),
            sandbox: SandboxMode::Off,
            network: false,
            allow_git_writes: false,
            test_commands: vec![],
            platform: "darwin".into(),
            sandbox_exec_path: String::new(),
            coordinator,
            coordination_wait_ms: 5000,
            environment: Some(Arc::new(HashMap::from([
                ("PATH".into(), process_env().remove("PATH").unwrap()),
                ("HOME".into(), root.to_string_lossy().into_owned()),
            ]))),
        }));
        Self { dir, env, waiting }
    }

    async fn hold(&self) -> crate::coordination::WorkspaceLease {
        self.env
            .coordinator
            .acquire_workspace(AcquireWorkspaceInput {
                root: self.env.root().await.unwrap(),
                holder: LeaseHolder {
                    chat_id: None,
                    run_id: "holder".into(),
                    tool_call_id: None,
                    tool: "test".into(),
                    label: "held for regression".into(),
                },
                cancellation: None,
                max_wait_ms: Some(1000),
                execution_id: None,
            })
            .await
            .unwrap()
    }

    async fn assert_retired(&self) {
        let root = self.env.root().await.unwrap();
        let key = crate::coordination::canonical_root_key(&root).await;
        assert!(
            crate::coordination::read_intent_files(&key)
                .await
                .unwrap()
                .is_empty()
        );
        let mut lease = self.hold().await;
        lease.release().await;
    }
}

fn context(guard: Arc<Revocable>) -> ToolContext {
    ToolContext {
        run_id: "test-run".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: None,
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: agentkit_core::CancellationToken::new(),
        dispatch_guard: Some(guard),
        metadata: Map::new(),
        suspension: None,
    }
}

#[tokio::test]
async fn post_lease_revoked_dispatch_has_zero_effects_and_retires_intent() {
    let fixture = Fixture::new();
    let mut held = fixture.hold().await;
    let guard = Arc::new(Revocable(AtomicBool::new(false)));
    let ctx = context(guard.clone());
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo effect > effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    guard.0.store(true, Ordering::SeqCst);
    held.release().await;
    let result = pending.await.unwrap();
    assert!(!result.ok, "revoked authority must refuse: {result:?}");
    assert_eq!(
        result.data.as_ref().unwrap()["errorCode"],
        "command_not_started"
    );
    assert!(!fixture.dir.path().join("effect").exists());
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_unchanged_input_executes_once() {
    let fixture = Fixture::new();
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo effect >> effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    held.release().await;
    let result = pending.await.unwrap();
    assert!(result.ok, "{result:?}");
    assert!(
        result
            .warnings
            .iter()
            .all(|warning| !warning.contains("cleanup")),
        "positive control needs a clean process scan: {result:?}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.dir.path().join("effect")).unwrap(),
        "effect\n"
    );
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_pack_close_has_zero_effects() {
    let fixture = Fixture::new();
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo effect > effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    fixture.env.dispose().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
        .await
        .expect("pack close must cancel the queued command before its lease is released")
        .unwrap();
    assert!(!result.ok, "{result:?}");
    assert_eq!(
        result.data.as_ref().unwrap()["errorCode"],
        "command_not_started"
    );
    assert!(!fixture.dir.path().join("effect").exists());
    held.release().await;
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_cancelled_wait_has_zero_effects() {
    let fixture = Fixture::new();
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let cancellation = ctx.cancellation.clone();
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo effect > effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    cancellation.cancel();
    held.release().await;
    let result = pending.await.unwrap();
    assert!(!result.ok);
    assert!(!fixture.dir.path().join("effect").exists());
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_dropped_native_call_has_zero_effects() {
    let fixture = Fixture::new();
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo effect > effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    held.release().await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let key = crate::coordination::canonical_root_key(&fixture.env.root().await.unwrap()).await;
    loop {
        if crate::coordination::read_intent_files(&key)
            .await
            .is_ok_and(|intents| intents.is_empty())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "dropped pre-spawn intent was not retired"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!fixture.dir.path().join("effect").exists());
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_changed_bash_env_contents_has_zero_effects() {
    let mut fixture = Fixture::new();
    let startup = fixture.dir.path().join("startup.sh");
    std::fs::write(&startup, "export CHECK=before\n").unwrap();
    let env = Arc::get_mut(&mut fixture.env).unwrap();
    Arc::make_mut(env.environment.as_mut().unwrap())
        .insert("BASH_ENV".into(), startup.to_string_lossy().into_owned());
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"echo check"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    std::fs::write(startup, "echo changed > effect\n").unwrap();
    held.release().await;
    let result = pending.await.unwrap();
    assert!(!result.ok, "{result:?}");
    assert_eq!(
        result.data.as_ref().unwrap()["errorCode"],
        "command_not_started"
    );
    assert!(!fixture.dir.path().join("effect").exists());
    fixture.assert_retired().await;
}

#[tokio::test]
async fn post_lease_changed_package_script_has_zero_effects() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("package.json"),
        r#"{"scripts":{"check":"node script.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        fixture.dir.path().join("script.js"),
        "require('node:fs').writeFileSync('effect', 'safe');",
    )
    .unwrap();
    let root = fixture.env.root().await.unwrap();
    crate::command_fingerprint::command_fingerprint(
        &root,
        &root,
        "npm run check",
        fixture.env.environment.as_ref().unwrap(),
    )
    .expect("package script identity resolves")
    .expect("package script identity is complete");
    let mut held = fixture.hold().await;
    let ctx = context(Arc::new(Revocable(AtomicBool::new(false))));
    let tool = run_command_tool(&fixture.env);
    let pending = tokio::spawn(async move {
        tool.execute(ctx, json!({"command":"npm run check"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.waiting.0.notified(),
    )
    .await
    .unwrap();
    std::fs::write(
        fixture.dir.path().join("script.js"),
        "require('node:fs').writeFileSync('effect', 'changed');",
    )
    .unwrap();
    held.release().await;
    let result = pending.await.unwrap();
    assert!(!result.ok, "changed script must refuse: {result:?}");
    assert!(!fixture.dir.path().join("effect").exists());
    fixture.assert_retired().await;
}
