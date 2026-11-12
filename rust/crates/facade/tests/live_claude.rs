//! Opt-in live qualification of the facade against the REAL `claude` CLI.
//!
//! Skipped (returns early) unless `AGENTKIT_LIVE_CLAUDE=1`; additionally
//! `#[ignore]`d so a plain `cargo test` never spends tokens. Each turn costs
//! subscription quota on the pinned Haiku 5.5 model. Output never includes account
//! identity or credentials.
//!
//! ```text
//! AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-facade --test live_claude -- --ignored --nocapture
//! ```

use agentkit_claude::discovery::ClaudeSupportMatrix;
use agentkit_facade::{
    AuthExpectation, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION, ExecutableEvidence,
    NativeChat, NativeExecutionTarget, NativeProfile, NativeTerminalOutcome, TurnInput, TurnResume,
};

struct HaikuOnlySupervisor(agentkit_claude::transport::OwnedProcessSupervisor);

#[async_trait::async_trait]
impl agentkit_facade::ProcessSupervisor for HaikuOnlySupervisor {
    async fn spawn(
        &self,
        plan: &agentkit_facade::LaunchPlan,
        limits: &agentkit_facade::ResourceLimits,
        cancel: &agentkit_facade::CancellationToken,
    ) -> Result<Box<dyn agentkit_facade::OwnedChildHandle>, agentkit_facade::NativeError> {
        let mut plan = plan.clone();
        // The gate must prevent fallback, rather than detect it after spending quota.
        plan.argv
            .extend(["--fallback-model".into(), "claude-haiku-5-5".into()]);
        std::fs::write(
            plan.per_run_dir.join("settings.json"),
            br#"{"fallbackModel":[],"availableModels":["claude-haiku-5-5"]}"#,
        )
        .expect("private gate settings writable");
        self.0.spawn(&plan, limits, cancel).await
    }

    async fn drain_owned(
        &self,
        policy: agentkit_facade::InterruptPolicy,
    ) -> Vec<agentkit_facade::InterruptReceipt> {
        self.0.drain_owned(policy).await
    }

    async fn terminate_owned(
        &self,
        pid: u32,
        generation: u64,
        policy: agentkit_facade::InterruptPolicy,
    ) -> Option<agentkit_facade::InterruptReceipt> {
        self.0.terminate_owned(pid, generation, policy).await
    }

    fn descendant_cleanup(
        &self,
        pid: u32,
        generation: u64,
    ) -> Option<agentkit_facade::InterruptReceipt> {
        self.0.descendant_cleanup(pid, generation)
    }
}

fn target(evidence: &ExecutableEvidence) -> NativeExecutionTarget {
    NativeExecutionTarget {
        schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
        backend: "claude-code".into(),
        backend_version: Some(evidence.version.clone()),
        profile: NativeProfile::PlainChat,
        executable: evidence.path.display().to_string(),
        requested_model: Some("claude-haiku-5-5".into()),
        requested_effort: None,
        auth_expectation: AuthExpectation::SubscriptionLogin,
        capabilities: ClaudeSupportMatrix::snapshot(Some(&evidence.version)),
        config_identity: "live-qualification".into(),
        system_prompt_revision: Some("r1".into()),
        content: ContentPolicy::default(),
    }
}

fn input(id: &str, text: &str, resume: TurnResume) -> TurnInput {
    TurnInput {
        input_id: id.into(),
        text: text.into(),
        context_blocks: vec![],
        resume,
        system_prompt_revision: Some("r1".into()),
        system_prompt: Some("You are a terse assistant.".into()),
        max_vendor_turns: None,
        turn_deadline_ms: 120_000,
    }
}

/// Hyphenated UUID shape: 8-4-4-4-12 hex groups.
fn is_hyphenated_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && groups
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

#[tokio::test]
#[ignore = "live: needs a signed-in claude CLI; run with AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-facade --test live_claude -- --ignored"]
async fn live_fresh_turn_then_exact_resume() {
    if std::env::var("AGENTKIT_LIVE_CLAUDE").as_deref() != Ok("1") {
        eprintln!("skipped: AGENTKIT_LIVE_CLAUDE != 1");
        return;
    }
    let run_root = tempfile::tempdir().unwrap();
    let chat = NativeChat::new(None, run_root.path()).with_supervisor(std::sync::Arc::new(
        HaikuOnlySupervisor(agentkit_claude::transport::OwnedProcessSupervisor::new()),
    ));
    let report = chat.preflight().await.expect("preflight succeeds");
    let target = target(&report.executable);

    let fresh = chat
        .chat_turn(
            &target,
            &input(
                "live-1",
                "Reply with exactly: pong",
                TurnResume::Fresh {
                    seed_history: vec![],
                },
            ),
        )
        .await
        .expect("fresh turn settles");
    let session_id = match fresh {
        NativeTerminalOutcome::Completed {
            text,
            session_id,
            observed_model,
            ..
        } => {
            assert_eq!(observed_model.as_deref(), Some("claude-haiku-5-5"));
            assert!(text.to_lowercase().contains("pong"), "reply: {text}");
            assert!(
                is_hyphenated_uuid(&session_id),
                "session id must be a hyphenated UUID: {session_id}"
            );
            session_id
        }
        other => panic!("fresh turn must complete, got {other:?}"),
    };

    let resumed = chat
        .chat_turn(
            &target,
            &input(
                "live-2",
                "What single word did you reply with previously?",
                TurnResume::Exact {
                    vendor_session_id: session_id.clone(),
                },
            ),
        )
        .await
        .expect("resumed turn settles");
    match resumed {
        NativeTerminalOutcome::Completed {
            text,
            session_id: resumed_session,
            observed_model,
            ..
        } => {
            assert_eq!(observed_model.as_deref(), Some("claude-haiku-5-5"));
            assert_eq!(
                resumed_session, session_id,
                "exact resume keeps the session"
            );
            assert!(text.to_lowercase().contains("pong"), "reply: {text}");
        }
        other => panic!("exact resume must complete, got {other:?}"),
    }
}
