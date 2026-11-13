//! The `NativeBackend` composition for the official Claude Code executable
//! (AGENTKIT-109/111; consumed generically by AGENTKIT-113's executor).
//!
//! Composes discovery (preflight), launch profiles (plan), protocol
//! (input encoding, framing adapter, terminal mapping). Holds no durable
//! state and never touches credentials: authentication is CLI-owned.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use agentkit_native::{
    AuthExpectation, AuthMode, ExecutableLocator, LaunchPlan, McpEndpointSpec, NativeBackend,
    NativeExecutionTarget, NativeRefusal, NativeTerminalOutcome, PolicyState, PreflightReport,
    ProcessExit, StreamFrameAdapter, TurnInput,
};

use crate::discovery::{ClaudeSupportMatrix, recheck_replaced};
use crate::launch::{BACKEND_ID, build_launch_plan, new_per_run_dir, write_per_run_files};
use crate::protocol::{ClaudeStreamAdapter, claude_input_frame, claude_terminal_outcome};

/// Driver entry point. `run_root` parents private per-attempt run
/// directories (removed at attempt cleanup by the host).
pub struct ClaudeCodeBackend {
    run_root: PathBuf,
}

impl ClaudeCodeBackend {
    pub fn new(run_root: impl Into<PathBuf>) -> Self {
        ClaudeCodeBackend {
            run_root: run_root.into(),
        }
    }

    /// Default: `TMPDIR/agentkit-claude-runs` (per-user private via the
    /// per-run directory's own 0700 mode).
    pub fn with_temp_run_root() -> Self {
        Self::new(std::env::temp_dir().join("agentkit-claude-runs"))
    }

    fn check_cancel(cancel: &CancellationToken) -> Result<(), NativeRefusal> {
        if cancel.is_cancelled() {
            return Err(NativeRefusal::new("cancelled", "preflight cancelled", true));
        }
        Ok(())
    }
}

#[async_trait]
impl NativeBackend for ClaudeCodeBackend {
    fn backend_id(&self) -> &str {
        BACKEND_ID
    }

    async fn preflight(
        &self,
        target: &NativeExecutionTarget,
        locator: &dyn ExecutableLocator,
        cancel: &CancellationToken,
    ) -> Result<PreflightReport, NativeRefusal> {
        Self::check_cancel(cancel)?;
        if target.backend != BACKEND_ID {
            return Err(NativeRefusal::new(
                "backend_mismatch",
                format!(
                    "driver is for '{BACKEND_ID}', target says '{}'",
                    target.backend
                ),
                false,
            ));
        }

        let executable = locator.locate().await?;
        Self::check_cancel(cancel)?;

        // Probe and spawn must target the same canonical executable the
        // admission froze.
        if executable.path != std::path::Path::new(&target.executable) {
            return Err(NativeRefusal::new(
                "executable_mismatch",
                format!(
                    "admission froze '{}', discovery resolves '{}'",
                    target.executable,
                    executable.path.display()
                ),
                true,
            ));
        }
        // Replacement between probe and now (AGENTKIT-109).
        recheck_replaced(&executable)?;

        // Compatibility gate only (AGENTKIT-116 qualification-honesty
        // lane): preflight refuses below-floor/unparseable versions; the
        // qualified verdict itself requires recorded qualification
        // evidence (`ClaudeSupportMatrix::is_qualified`), never a version
        // observation.
        if !ClaudeSupportMatrix::meets_qualified_floor(&executable.version) {
            return Err(NativeRefusal::new(
                "unsupported_version",
                format!(
                    "observed version '{}' is below the qualified floor or unparseable",
                    executable.version
                ),
                false,
            ));
        }

        if let Some(admitted_version) = &target.backend_version
            && admitted_version != &executable.version
        {
            return Err(NativeRefusal::new(
                "executable_version_mismatch",
                format!(
                    "admission froze version '{admitted_version}', execution observed '{}'; readmit the turn",
                    executable.version
                ),
                false,
            ));
        }

        let auth = locator.auth_status().await?;
        Self::check_cancel(cancel)?;

        let mut readiness_notes = Vec::new();
        match (target.auth_expectation, &auth.mode) {
            (_, AuthMode::None) => {
                return Err(NativeRefusal::new(
                    "not_signed_in",
                    "official status reports no sign-in; run `claude auth login` (CLI-owned)",
                    true,
                ));
            }
            (AuthExpectation::SubscriptionLogin, AuthMode::Subscription { .. })
            | (AuthExpectation::ApiAuth, AuthMode::ApiKey | AuthMode::ApiKeyHelper) => {}
            (AuthExpectation::AsConfigured, _) => {}
            (AuthExpectation::SubscriptionLogin, AuthMode::Unknown)
            | (AuthExpectation::ApiAuth, AuthMode::Unknown) => {
                return Err(NativeRefusal::new(
                    "auth_status_unavailable",
                    "official auth status is not observable for the requested expectation",
                    true,
                ));
            }
            (expected, mode) => {
                return Err(NativeRefusal::new(
                    "auth_expectation_mismatch",
                    format!("target expects {expected:?}, official status reports {mode:?}"),
                    true,
                ));
            }
        }
        if matches!(auth.mode, AuthMode::Subscription { .. }) {
            // Honest surfacing: subscription auth proves neither entitlement
            // nor quota nor token refresh (AGENTKIT-109). Informational —
            // the CLI-owned subscription is the supported auth route, so
            // this note must NOT gate the spawn.
            readiness_notes.push(
                "subscription sign-in observed; model entitlement and billing inclusion are \
                 not verified by preflight"
                    .to_owned(),
            );
        }
        match auth.policy {
            PolicyState::Conflicting => {
                return Err(NativeRefusal::new(
                    "managed_policy_conflict",
                    "managed policy restricts this launch profile; restrictions are respected, \
                     never bypassed",
                    false,
                ));
            }
            PolicyState::Unknown => {
                readiness_notes.push("managed policy state could not be observed".to_owned())
            }
            PolicyState::PresentCompatible => {
                readiness_notes.push("managed policy file present; launch respects it".to_owned())
            }
            PolicyState::None => {}
        }

        Ok(PreflightReport {
            executable,
            auth,
            // Every blocking state above is a `NativeRefusal`; reaching the
            // report means the launch is admissible.
            blocking_issues: Vec::new(),
            readiness_notes,
        })
    }

    fn plan_turn(
        &self,
        target: &NativeExecutionTarget,
        input: &TurnInput,
        mcp: Option<&McpEndpointSpec>,
    ) -> Result<LaunchPlan, NativeRefusal> {
        // Private per-attempt directory: pure validation first, then create
        // 0700 dir, stage 0600 files; any failure removes the directory.
        let per_run_dir = new_per_run_dir(&self.run_root);
        let plan = match build_launch_plan(target, input, mcp, per_run_dir.clone()) {
            Ok(plan) => plan,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&per_run_dir);
                return Err(e);
            }
        };
        // Stable cwd shared by every attempt (see `build_launch_plan`);
        // created idempotently and NEVER removed by per-attempt cleanup.
        if let Err(e) = ensure_private_dir(&plan.cwd, "cwd") {
            let _ = std::fs::remove_dir_all(&per_run_dir);
            return Err(e);
        }
        if let Err(e) = stage_run_dir(&per_run_dir, input.system_prompt.as_deref(), mcp) {
            let _ = std::fs::remove_dir_all(&per_run_dir);
            return Err(e);
        }
        Ok(plan)
    }

    fn encode_input(&self, input: &TurnInput) -> Result<Vec<u8>, NativeRefusal> {
        // Seed history renders FIRST, then context blocks, then the new
        // user text; both render as inert labelled sections (AGENTKIT-113:
        // seed/context are data, never instructions to execute). Exact
        // resume carries no seed, so the order degrades to today's shape.
        let mut prompt = String::new();
        let seed_blocks: &[agentkit_native::ContextBlock] = match &input.resume {
            agentkit_native::TurnResume::Fresh { seed_history } => seed_history,
            _ => &[],
        };
        for block in seed_blocks.iter().chain(input.context_blocks.iter()) {
            prompt.push_str("<agentkit-context kind=\"");
            prompt.push_str(&block.kind);
            prompt.push_str("\" identity=\"");
            prompt.push_str(&block.content_identity);
            prompt.push_str("\">\n");
            prompt.push_str(&block.text);
            if !block.text.ends_with('\n') {
                prompt.push('\n');
            }
            prompt.push_str("</agentkit-context>\n");
        }
        prompt.push_str(&input.text);
        claude_input_frame(&prompt)
    }

    fn new_frame_adapter(&self) -> Box<dyn StreamFrameAdapter> {
        Box::new(ClaudeStreamAdapter::new())
    }

    fn terminal_outcome(
        &self,
        exit: &ProcessExit,
        observed: &[agentkit_native::NativeEvent],
        partial_text: Option<&str>,
    ) -> NativeTerminalOutcome {
        claude_terminal_outcome(exit, observed, partial_text)
    }
}

/// Create a directory (0700), idempotently, tightening the leaf mode.
fn ensure_private_dir(dir: &std::path::Path, field: &'static str) -> Result<(), NativeRefusal> {
    // `agentkit-sys` names the failing step in the error text ("cannot
    // create …" / "cannot tighten …"); that text is the refusal detail.
    agentkit_sys::create_private_dir(dir)
        .map_err(|e| NativeRefusal::new(field, e.to_string(), true))
}

/// Create the private run directory (0700) and stage its files (0600).
fn stage_run_dir(
    per_run_dir: &std::path::Path,
    system_prompt: Option<&str>,
    mcp: Option<&McpEndpointSpec>,
) -> Result<(), NativeRefusal> {
    ensure_private_dir(per_run_dir, "per_run_dir")?;
    write_per_run_files(per_run_dir, system_prompt, mcp)
}

/// Convenience constructor used by standalone consumers: backend wired with
/// the default locator (explicit path or PATH discovery).
pub fn default_backend(
    explicit_executable: Option<PathBuf>,
    run_root: impl Into<PathBuf>,
) -> (ClaudeCodeBackend, Arc<dyn ExecutableLocator>) {
    use crate::discovery::ClaudeLocator;
    let locator: Arc<dyn ExecutableLocator> = Arc::new(ClaudeLocator::new(explicit_executable));
    (ClaudeCodeBackend::new(run_root), locator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::tests_exports::{plain_target, turn_input};
    use agentkit_native::{
        AuthEvidence, ExecutableEvidence, NativeEvent, NativeProfile, ProcessExit, TurnResume,
    };

    struct ScriptedLocator {
        evidence: Result<ExecutableEvidence, NativeRefusal>,
        auth: Result<AuthEvidence, NativeRefusal>,
    }

    #[async_trait]
    impl ExecutableLocator for ScriptedLocator {
        async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
            self.evidence.clone()
        }
        async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
            self.auth.clone()
        }
    }

    /// A real fixture file: preflight's replacement recheck must observe an
    /// existing executable, exactly like the qualified floor's live path.
    static GOOD_PATH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let dir = std::env::temp_dir().join(format!("ak-claude-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("claude");
        std::fs::write(&p, b"official-cli-fixture").unwrap();
        p.to_string_lossy().into_owned()
    });

    fn evidence() -> ExecutableEvidence {
        ExecutableEvidence {
            path: GOOD_PATH.clone().into(),
            version: "2.1.310".into(),
            digest: None,
        }
    }

    fn auth_subscription() -> AuthEvidence {
        AuthEvidence {
            mode: AuthMode::Subscription {
                account_scope: "claude.ai".into(),
            },
            policy: PolicyState::None,
            config_directory: None,
        }
    }

    fn backend_in(tmp: &tempfile::TempDir) -> ClaudeCodeBackend {
        ClaudeCodeBackend::new(tmp.path().join("runs"))
    }

    fn preflight_with(
        target: &NativeExecutionTarget,
        loc: ScriptedLocator,
    ) -> Result<PreflightReport, NativeRefusal> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            backend_in(&tempfile::tempdir().unwrap())
                .preflight(target, &loc, &CancellationToken::new())
                .await
        })
    }

    #[test]
    fn preflight_happy_path_carries_evidence_and_honest_notes() {
        let t = plain_target(&GOOD_PATH);
        let report = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(auth_subscription()),
            },
        )
        .unwrap();
        assert_eq!(report.executable.version, "2.1.310");
        // Informational honesty notes are surfaced…
        assert!(
            report
                .readiness_notes
                .iter()
                .any(|i| i.contains("entitlement"))
        );
        // …but a properly signed-in subscription user is NEVER blocked:
        // the executor refuses only on `blocking_issues` (P1-1 regression).
        assert!(report.blocking_issues.is_empty());
    }

    #[test]
    fn preflight_rejects_frozen_version_drift() {
        let mut target = plain_target(&GOOD_PATH);
        target.backend_version = Some("2.1.259".into());
        let error = preflight_with(
            &target,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(auth_subscription()),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "executable_version_mismatch");
        assert!(error.message.contains("readmit"));
    }

    #[test]
    fn preflight_refusals_are_specific() {
        let t = plain_target(&GOOD_PATH);
        // Path drift from the frozen admission.
        let mut drift = evidence();
        drift.path = "/opt/claude".into();
        let err = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(drift),
                auth: Ok(auth_subscription()),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "executable_mismatch");

        // Below qualified floor.
        let mut old = evidence();
        old.version = "2.0.14".into();
        let err = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(old),
                auth: Ok(auth_subscription()),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "unsupported_version");

        // Signed out.
        let mut signed_out = auth_subscription();
        signed_out.mode = AuthMode::None;
        let err = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(signed_out),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "not_signed_in");
        assert!(err.retryable);

        // Expectation crossing: wants subscription, status says API key.
        let mut api = auth_subscription();
        api.mode = AuthMode::ApiKey;
        let err = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(api.clone()),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "auth_expectation_mismatch");
        // AsConfigured accepts it.
        let mut t2 = plain_target(&GOOD_PATH);
        t2.auth_expectation = AuthExpectation::AsConfigured;
        preflight_with(
            &t2,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(api),
            },
        )
        .unwrap();

        // Managed conflict.
        let mut conflicting = auth_subscription();
        conflicting.policy = PolicyState::Conflicting;
        let err = preflight_with(
            &t,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(conflicting),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "managed_policy_conflict");

        // Cancellation before any probe.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(ClaudeCodeBackend::new("/tmp/unused").preflight(
                &t,
                &evidence_locator(),
                &cancel,
            ))
            .unwrap_err();
        assert_eq!(err.code, "cancelled");

        // Wrong backend id.
        let mut other = plain_target(&GOOD_PATH);
        other.backend = "codex".into();
        let err = preflight_with(
            &other,
            ScriptedLocator {
                evidence: Ok(evidence()),
                auth: Ok(auth_subscription()),
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "backend_mismatch");
    }

    fn evidence_locator() -> ScriptedLocator {
        ScriptedLocator {
            evidence: Ok(evidence()),
            auth: Ok(auth_subscription()),
        }
    }

    #[test]
    fn plan_turn_stages_private_dir_and_binds_session() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(&tmp);
        let t = plain_target(&GOOD_PATH);
        let input = turn_input(TurnResume::Fresh {
            seed_history: vec![],
        });
        let plan = backend.plan_turn(&t, &input, None).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&plan.per_run_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        for name in ["mcp.json", "settings.json", "prompt.txt"] {
            let p = plan.per_run_dir.join(name);
            let md = std::fs::metadata(&p).unwrap();
            assert_eq!(md.permissions().mode() & 0o777, 0o600, "{name}");
        }
        assert!(
            plan.argv
                .contains(&plan.per_run_dir.join("prompt.txt").display().to_string())
        );
        assert!(plan.native_session_id.is_some());
        // Stable cwd: created 0700, sibling of the per-run dir, shared by
        // attempts, and NOT removed with the per-run dir.
        assert_eq!(plan.cwd, tmp.path().join("runs").join("cwd"));
        assert_ne!(plan.cwd, plan.per_run_dir);
        assert_eq!(
            std::fs::metadata(&plan.cwd).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let plan2 = backend.plan_turn(&t, &input, None).unwrap();
        assert_eq!(plan2.cwd, plan.cwd);
        assert_ne!(plan2.per_run_dir, plan.per_run_dir);
        std::fs::remove_dir_all(&plan.per_run_dir).unwrap();
        assert!(plan.cwd.is_dir());
        assert_eq!(
            plan.argv.iter().filter(|a| **a == "--session-id").count(),
            1
        );
        // Under run_root: the stable `cwd` plus exactly one live private
        // per-attempt dir (the first was removed above, plan2's remains).
        let mut names: Vec<String> = std::fs::read_dir(tmp.path().join("runs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(names[1], "cwd");
        assert!(names[0].starts_with(crate::launch::PRIVATE_RUN_DIR_PREFIX));
    }

    #[test]
    fn plan_turn_failure_leaves_no_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(&tmp);
        // Plain profile with an MCP endpoint must refuse before creating.
        let t = plain_target(&GOOD_PATH);
        let input = turn_input(TurnResume::Fresh {
            seed_history: vec![],
        });
        let broker = McpEndpointSpec {
            server_name: "onemind".into(),
            broker_tool_names: vec!["mcp__onemind__x".into()],
            stdio_command: Some(vec!["/bin/bridge".into()]),
            http_url: None,
            auth_token: None,
        };
        let err = backend.plan_turn(&t, &input, Some(&broker)).unwrap_err();
        assert_eq!(err.code, "mcp_on_plain_profile");
        assert!(
            !tmp.path().join("runs").exists() || {
                std::fs::read_dir(tmp.path().join("runs"))
                    .map(|d| d.count())
                    .unwrap_or(0)
                    == 0
            }
        );
    }

    #[test]
    fn plan_turn_mcp_profile_stages_broker_config() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(&tmp);
        let mut t = plain_target(&GOOD_PATH);
        t.profile = NativeProfile::OwnedMcpChat;
        t.capabilities.flags.insert(
            "owned-mcp-chat".into(),
            agentkit_native::CapabilityState::Supported {
                qualified_version: "2.1.310".into(),
                evidence: "test".into(),
            },
        );
        let input = turn_input(TurnResume::Exact {
            vendor_session_id: "sess-9".into(),
        });
        let broker = McpEndpointSpec {
            server_name: "onemind".into(),
            broker_tool_names: vec!["mcp__onemind__search".into()],
            stdio_command: Some(vec!["/bin/bridge".into(), "--attempt".into()]),
            http_url: None,
            auth_token: None,
        };
        let plan = backend.plan_turn(&t, &input, Some(&broker)).unwrap();
        let cfg = std::fs::read_to_string(plan.per_run_dir.join("mcp.json")).unwrap();
        assert!(cfg.contains("onemind"));
        assert!(cfg.contains("\"/bin/bridge\""));
        assert_eq!(plan.native_session_id.as_deref(), Some("sess-9"));
        assert!(plan.argv.contains(&"--resume".to_string()));
    }

    #[test]
    fn encode_input_wraps_context_as_inert_data() {
        let backend = ClaudeCodeBackend::new("/tmp/unused");
        let mut input = turn_input(TurnResume::Fresh {
            seed_history: vec![],
        });
        input.context_blocks.push(agentkit_native::ContextBlock {
            kind: "selected-source".into(),
            content_identity: "note:42@r3".into(),
            text: "selected text".into(),
        });
        let bytes = backend.encode_input(&input).unwrap();
        let frame = String::from_utf8(bytes).unwrap();
        assert!(frame.starts_with("{\"type\":\"user\""));
        assert!(frame.ends_with('\n'));
        assert!(frame.contains("agentkit-context"));
        assert!(frame.contains("note:42@r3"));
        assert!(frame.contains("selected text"));
        assert!(frame.contains(&input.text));
        // The prompt never lands in argv-style form here; it is stdin bytes.
        assert!(frame.contains("\\n") || frame.contains('\n'));
    }

    #[test]
    fn encode_input_renders_seed_history_before_context_and_user_text() {
        let backend = ClaudeCodeBackend::new("/tmp/unused");
        let mut input = turn_input(TurnResume::Fresh {
            seed_history: vec![agentkit_native::ContextBlock {
                kind: "seed-history".into(),
                content_identity: "msg:seed-1@r1".into(),
                text: "SEED-SENTINEL-ancestry".into(),
            }],
        });
        input.context_blocks.push(agentkit_native::ContextBlock {
            kind: "selected-source".into(),
            content_identity: "note:42@r3".into(),
            text: "CTX-SENTINEL-context".into(),
        });
        input.text = "USER-TEXT-new".into();
        let bytes = backend.encode_input(&input).unwrap();
        let frame = String::from_utf8(bytes).expect("utf8 frame");
        assert!(frame.starts_with("{\"type\":\"user\",\"client_composed\":true"));
        // Inspect the decoded prompt: seed block, then context block, then
        // the user text — the seed sentinel must be ahead of both.
        let json: serde_json::Value = serde_json::from_str(frame.trim_end()).unwrap();
        let content = json["message"]["content"].as_str().expect("string content");
        let seed = content
            .find("SEED-SENTINEL-ancestry")
            .expect("seed sentinel rendered");
        let ctx = content
            .find("CTX-SENTINEL-context")
            .expect("context sentinel rendered");
        let user = content.find("USER-TEXT-new").expect("user text rendered");
        assert!(
            seed < ctx && ctx < user,
            "order must be seed, context, user: {content}"
        );
        assert!(content.contains("kind=\"seed-history\" identity=\"msg:seed-1@r1\""));
        // Exact resume renders no seed section at all.
        let exact = turn_input(TurnResume::Exact {
            vendor_session_id: "sess-9".into(),
        });
        let bytes = backend.encode_input(&exact).unwrap();
        let frame = String::from_utf8(bytes).unwrap();
        assert!(!frame.contains("seed-history"), "{frame}");
        assert!(frame.contains("hello"));
    }

    #[test]
    fn terminal_outcome_maps_exit_without_result_to_uncertain() {
        let backend = ClaudeCodeBackend::new("/tmp/unused");
        let exit = ProcessExit {
            pid: 7,
            generation: 1,
            exit_code: Some(0),
            signal: None,
            after_interrupt: false,
        };
        let outcome = backend.terminal_outcome(&exit, &[], None);
        assert!(
            matches!(outcome, NativeTerminalOutcome::Uncertain { .. }),
            "exit-0 without a root result can never be Completed: {outcome:?}"
        );
        let outcome = backend.terminal_outcome(&exit, &[], Some("half a sentence"));
        match outcome {
            NativeTerminalOutcome::Uncertain { partial_text, .. } => {
                assert_eq!(partial_text.as_deref(), Some("half a sentence"))
            }
            other => panic!("expected uncertain with partial, got {other:?}"),
        }
        // Silence about events shape here: the protocol worker owns the
        // result-detection matrix; backend only delegates.
        let _ = NativeEvent::TextDelta {
            text: String::new(),
        };
    }
}
