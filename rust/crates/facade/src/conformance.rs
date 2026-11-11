//! Driver conformance harness (AGENTKIT-117).
//!
//! Exercises the [`NativeBackend`] / [`ExecutableLocator`] ports generically
//! so any driver — the Claude Code driver and future ones — can be held to
//! the same non-negotiable behaviors before qualification: refusals are
//! explicit with stable codes and preserved retryability, argv never carries
//! token-shaped values, the frame adapter fails loudly on non-JSON while
//! tolerating qualified additive frames, and a clean exit without a root
//! result can NEVER produce `Completed` (AGENTKIT-107 §7).
//!
//! Case ids are stable snake_case identifiers; renaming one is a breaking
//! change for qualification records. Every case reports pass/fail with a
//! human-readable detail.

use agentkit_native::{
    AuthMode, ExecutableLocator, FinishDisposition, NativeBackend, NativeExecutionTarget,
    NativeProfile, ProcessExit, TurnInput, TurnResume,
};
use tokio_util::sync::CancellationToken;

/// One required conformance case. `id` is the stable snake_case identifier
/// recorded in qualification reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceCase {
    pub id: &'static str,
    pub title: &'static str,
}

/// Result of executing one case against a driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceReport {
    pub case_id: String,
    pub title: String,
    pub passed: bool,
    pub detail: String,
}

impl ConformanceReport {
    fn ok(case: &ConformanceCase, detail: impl Into<String>) -> Self {
        ConformanceReport {
            case_id: case.id.to_owned(),
            title: case.title.to_owned(),
            passed: true,
            detail: detail.into(),
        }
    }

    fn fail(case: &ConformanceCase, detail: impl Into<String>) -> Self {
        ConformanceReport {
            case_id: case.id.to_owned(),
            title: case.title.to_owned(),
            passed: false,
            detail: detail.into(),
        }
    }
}

/// The full required case set, in reporting order.
pub fn required_cases() -> Vec<ConformanceCase> {
    vec![
        ConformanceCase {
            id: "backend_identity_stable",
            title: "backend identity is non-empty and stable across calls and targets",
        },
        ConformanceCase {
            id: "preflight_carries_executable_and_auth_evidence",
            title: "preflight reports executable identity plus an observable auth mode",
        },
        ConformanceCase {
            id: "plan_turn_refuses_foreign_backend",
            title: "plan_turn refuses a target naming a foreign backend",
        },
        ConformanceCase {
            id: "plan_turn_refuses_capability_unknown_profile_flag",
            title: "plan_turn refuses an unknown profile capability flag with retryable \
                   'capability_unknown'",
        },
        ConformanceCase {
            id: "plan_turn_refuses_owned_mcp_without_endpoint",
            title: "plan_turn refuses OwnedMcpChat without a scoped endpoint spec",
        },
        ConformanceCase {
            id: "plan_turn_refuses_exact_resume_without_session_id",
            title: "plan_turn refuses an Exact resume with an empty vendor session id",
        },
        ConformanceCase {
            id: "argv_never_contains_token_shaped_value",
            title: "a frozen plan's argv never contains token-shaped values (redacted_argv \
                   is a no-op on clean argv)",
        },
        ConformanceCase {
            id: "encode_input_non_empty",
            title: "encode_input yields a non-empty framed stdin payload",
        },
        ConformanceCase {
            id: "frame_adapter_additive_tolerance_and_non_json_rejection",
            title: "frame adapter tolerates qualified additive frames and errors on non-JSON",
        },
        ConformanceCase {
            id: "terminal_outcome_never_completed_without_root_result",
            title: "exit code 0 without any root result never maps to Completed",
        },
    ]
}

/// Run the given cases against a driver through the shared ports.
///
/// `target_factory` must produce a *valid* PlainChat target for `backend`
/// (a fresh value per call; cases mutate their own copies). The harness
/// cleans up any private run directory it stages.
pub async fn run_driver_conformance(
    backend: &dyn NativeBackend,
    locator: &dyn ExecutableLocator,
    target_factory: fn() -> NativeExecutionTarget,
    cases: &[ConformanceCase],
) -> Vec<ConformanceReport> {
    let mut reports = Vec::with_capacity(cases.len());
    for case in cases {
        let outcome = run_case(backend, locator, target_factory, case).await;
        reports.push(match outcome {
            Ok(detail) => ConformanceReport::ok(case, detail),
            Err(detail) => ConformanceReport::fail(case, detail),
        });
    }
    reports
}

/// One admitted probe input shared by the plan/encode cases.
fn probe_input() -> TurnInput {
    TurnInput {
        input_id: "conformance-input-1".to_owned(),
        text: "conformance probe".to_owned(),
        context_blocks: Vec::new(),
        resume: TurnResume::Fresh {
            seed_history: Vec::new(),
        },
        system_prompt_revision: None,
        system_prompt: None,
        max_vendor_turns: None,
        turn_deadline_ms: 60_000,
    }
}

fn token_shaped(arg: &str) -> bool {
    let lower = arg.to_lowercase();
    lower.contains("token") || lower.contains("key=") || lower.contains("secret")
}

async fn run_case(
    backend: &dyn NativeBackend,
    locator: &dyn ExecutableLocator,
    target_factory: fn() -> NativeExecutionTarget,
    case: &ConformanceCase,
) -> Result<String, String> {
    match case.id {
        "backend_identity_stable" => {
            let first = backend.backend_id();
            let second = backend.backend_id();
            let target = target_factory();
            if first.is_empty() {
                return Err("backend_id is empty".to_owned());
            }
            if first != second {
                return Err(format!("backend_id unstable: '{first}' vs '{second}'"));
            }
            if first != target.backend {
                return Err(format!(
                    "backend_id '{first}' disagrees with the target backend '{}'",
                    target.backend
                ));
            }
            Ok(format!("stable identity '{first}'"))
        }
        "preflight_carries_executable_and_auth_evidence" => {
            let target = target_factory();
            let report = backend
                .preflight(&target, locator, &CancellationToken::new())
                .await
                .map_err(|refusal| format!("preflight refused: {refusal}"))?;
            if report.executable.version.trim().is_empty() {
                return Err("preflight carries no executable version evidence".to_owned());
            }
            if report.executable.path.as_os_str().is_empty() {
                return Err("preflight carries no executable path evidence".to_owned());
            }
            if matches!(report.auth.mode, AuthMode::Unknown | AuthMode::None) {
                return Err(format!(
                    "preflight reports unobservable auth: {:?}",
                    report.auth.mode
                ));
            }
            Ok(format!(
                "executable {} @ {}; auth observable",
                report.executable.version,
                report.executable.path.display()
            ))
        }
        "plan_turn_refuses_foreign_backend" => {
            let mut target = target_factory();
            target.backend = "foreign-driver-not-qualified".to_owned();
            match backend.plan_turn(&target, &probe_input(), None) {
                Ok(plan) => {
                    let _ = std::fs::remove_dir_all(&plan.per_run_dir);
                    Err("accepted a target naming a foreign backend".to_owned())
                }
                Err(refusal) => Ok(format!(
                    "refused '{}' retryable={}",
                    refusal.code, refusal.retryable
                )),
            }
        }
        "plan_turn_refuses_capability_unknown_profile_flag" => {
            let mut target = target_factory();
            // Drop the profile flag so its state degrades to Unknown: the
            // driver must refuse explicitly, retryably, never guess "yes".
            target.capabilities.flags.remove("plain-chat");
            match backend.plan_turn(&target, &probe_input(), None) {
                Ok(plan) => {
                    let _ = std::fs::remove_dir_all(&plan.per_run_dir);
                    Err("accepted a target with an unknown profile capability flag".to_owned())
                }
                Err(refusal) => {
                    if refusal.code != "capability_unknown" {
                        return Err(format!(
                            "expected code 'capability_unknown', got '{}'",
                            refusal.code
                        ));
                    }
                    if !refusal.retryable {
                        return Err("'capability_unknown' must be retryable=true".to_owned());
                    }
                    Ok(format!(
                        "refused capability_unknown retryable=true: {}",
                        refusal.message
                    ))
                }
            }
        }
        "plan_turn_refuses_owned_mcp_without_endpoint" => {
            let mut target = target_factory();
            target.profile = NativeProfile::OwnedMcpChat;
            match backend.plan_turn(&target, &probe_input(), None) {
                Ok(plan) => {
                    let _ = std::fs::remove_dir_all(&plan.per_run_dir);
                    Err("accepted OwnedMcpChat without a scoped endpoint spec".to_owned())
                }
                Err(refusal) => Ok(format!(
                    "refused '{}' retryable={}",
                    refusal.code, refusal.retryable
                )),
            }
        }
        "plan_turn_refuses_exact_resume_without_session_id" => {
            let target = target_factory();
            let mut input = probe_input();
            input.resume = TurnResume::Exact {
                vendor_session_id: "   ".to_owned(),
            };
            match backend.plan_turn(&target, &input, None) {
                Ok(plan) => {
                    let _ = std::fs::remove_dir_all(&plan.per_run_dir);
                    Err("accepted an Exact resume with an empty vendor session id".to_owned())
                }
                Err(refusal) => Ok(format!(
                    "refused '{}' retryable={}",
                    refusal.code, refusal.retryable
                )),
            }
        }
        "argv_never_contains_token_shaped_value" => {
            let target = target_factory();
            let plan = backend
                .plan_turn(&target, &probe_input(), None)
                .map_err(|refusal| format!("plan_turn refused the valid probe: {refusal}"))?;
            let offenders: Vec<&String> =
                plan.argv.iter().filter(|arg| token_shaped(arg)).collect();
            let redacted = plan.redacted_argv();
            let result = if !offenders.is_empty() {
                Err(format!("argv carries token-shaped values: {offenders:?}"))
            } else if redacted != plan.argv {
                Err("redacted_argv altered an argv that was already clean".to_owned())
            } else {
                Ok(format!(
                    "{} argv entries, none token-shaped; redaction is a no-op on clean argv",
                    plan.argv.len()
                ))
            };
            let _ = std::fs::remove_dir_all(&plan.per_run_dir);
            result
        }
        "encode_input_non_empty" => {
            let bytes = backend
                .encode_input(&probe_input())
                .map_err(|refusal| format!("encode_input refused: {refusal}"))?;
            if bytes.is_empty() {
                return Err("encode_input produced an empty payload".to_owned());
            }
            Ok(format!("{} stdin bytes", bytes.len()))
        }
        "frame_adapter_additive_tolerance_and_non_json_rejection" => {
            let mut adapter = backend.new_frame_adapter();
            if let Err(violation) = adapter.push_line(br#"{"type":"plugin_install"}"#) {
                return Err(format!(
                    "qualified additive top-level frame rejected: {}: {}",
                    violation.code, violation.message
                ));
            }
            if let Err(violation) =
                adapter.push_line(br#"{"type":"system","subtype":"hook_progress"}"#)
            {
                return Err(format!(
                    "qualified additive system frame rejected: {}: {}",
                    violation.code, violation.message
                ));
            }
            match adapter.push_line(b"this is not json") {
                Ok(_) => Err("non-JSON frame was silently tolerated".to_owned()),
                Err(violation) => Ok(format!(
                    "additive frames tolerated; non-JSON refused as '{}'",
                    violation.code
                )),
            }
        }
        "terminal_outcome_never_completed_without_root_result" => {
            let mut adapter = backend.new_frame_adapter();
            let exit = ProcessExit {
                pid: 0,
                generation: 1,
                exit_code: Some(0),
                signal: None,
                after_interrupt: false,
            };
            let FinishDisposition::NoResult { partial_text } = adapter.finish();
            if partial_text.is_some() {
                return Err(
                    "finish() retained partial text on a stream that saw no events".to_owned(),
                );
            }
            match adapter.terminal_outcome(&exit) {
                agentkit_native::NativeTerminalOutcome::Completed { .. } => {
                    Err("clean exit without any root result produced Completed".to_owned())
                }
                other => Ok(format!("exit(0) without root result mapped to {other:?}")),
            }
        }
        other => Err(format!("unknown conformance case id '{other}'")),
    }
}
