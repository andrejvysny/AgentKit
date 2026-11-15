//! TU-23: a call through the common dialect equals the canonical call and
//! receives the same guard verdict as the direct call. Every real coding
//! tool, every example call, every mode and role.
//!
//! The guard sees the call the call pipeline hands it: the translated one,
//! with the registered (canonical) tool definition. This test builds that
//! context for both calls and compares what the coding permission guard says.

use std::sync::Arc;

use agentkit_core::{
    CancellationToken, Dialect, Tool, ToolDefinition, ToolLimits, ToolRegistry, anthropic_dialect,
    builtin_tool_type, common_dialect,
};
use agentkit_governance::{
    ContributedTool, ToolContributionContext, ToolContributor, ToolGuard, ToolGuardContext,
    ToolGuardVerdict,
};
use agentkit_toolpacks_coding::{
    CODING_NAMESPACE, CODING_TOOL_NAMES, CodingLimitsOverride, CodingMode, CodingPermissionGuard,
    CodingPermissionGuardOptions, CodingRole, CodingToolPackOptions, CodingWorkspace, SandboxMode,
    create_coding_tool_pack,
};
use serde_json::{Value, json};

fn contribution() -> ToolContributionContext {
    ToolContributionContext {
        durable: None,
        chat_id: Some("chat-1".into()),
        run_id: "run-1".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits {
            profile: "small".into(),
            max_bytes: 16384,
            max_items: None,
        },
        cancellation: CancellationToken::new(),
        has_primary_binding: true,
        timeout_ms: 30_000,
    }
}

async fn coding_tools() -> Vec<Arc<dyn Tool>> {
    let pack = create_coding_tool_pack(CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: "/tmp".into(),
        },
        limits: CodingLimitsOverride::default(),
        allow_sensitive: false,
        clock: None,
        sandbox: SandboxMode::Off,
        network: false,
        allow_git_writes: false,
        test_commands: vec!["cargo test".into()],
        platform: Some("linux".into()),
        sandbox_exec_path: None,
        coordinator: None,
        coordination_wait_ms: None,
        environment: None,
        permission: None,
    })
    .await
    .expect("coding pack");
    pack.contribute(&contribution())
        .await
        .expect("contribute")
        .into_iter()
        .map(|tool| match tool {
            ContributedTool::Read(tool) | ContributedTool::DirectWrite(tool) => tool,
            ContributedTool::Write(_) => panic!("coding tools are not proposal-governed"),
        })
        .collect()
}

/// A registry frozen the way a new session freezes: presented in common v1.
async fn presented_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in coding_tools().await {
        registry.register(tool).expect("register");
    }
    let dialect = common_dialect();
    let offered = dialect.render(&registry.definitions());
    assert!(registry.freeze_presented(offered, vec![], dialect));
    registry
}

fn guard(mode: CodingMode, role: CodingRole) -> CodingPermissionGuard {
    CodingPermissionGuard::new(CodingPermissionGuardOptions {
        mode,
        role: Some(role),
        authority: None,
        plan_tool_names: None,
    })
}

fn shown(verdict: ToolGuardVerdict) -> String {
    match verdict {
        ToolGuardVerdict::Allowed => "allowed".into(),
        ToolGuardVerdict::Refused(reason) => format!("refused: {reason}"),
        ToolGuardVerdict::AwaitingHuman(ask) => {
            format!(
                "awaiting: {} / {:?} / {:?}",
                ask.reason, ask.preview, ask.requested_mode
            )
        }
    }
}

async fn verdict(guard: &CodingPermissionGuard, tool: &ToolDefinition, args: &Value) -> String {
    let context = ToolGuardContext {
        contribution: contribution(),
        namespace: CODING_NAMESPACE.to_owned(),
        tool: tool.clone(),
        arguments: Some(args.clone()),
    };
    shown(guard.can_execute(&context).await.expect("guard"))
}

const MODES: [CodingMode; 5] = [
    CodingMode::ReadOnly,
    CodingMode::Ask,
    CodingMode::AutoEdit,
    CodingMode::FullAuto,
    CodingMode::Plan,
];
const ROLES: [CodingRole; 3] = [
    CodingRole::Implementer,
    CodingRole::Reviewer,
    CodingRole::Supervisor,
];

#[tokio::test]
async fn every_coding_tool_is_offered_under_exactly_the_common_names() {
    let registry = presented_registry().await;
    let offered: Vec<String> = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(offered.len(), 14, "{offered:?}");
    assert_eq!(registry.names().len(), CODING_TOOL_NAMES.len());
    let mut expected = common_dialect().wire_names();
    expected.sort();
    let mut sorted = offered.clone();
    sorted.sort();
    assert_eq!(sorted, expected);
    for canonical in CODING_TOOL_NAMES {
        assert!(
            !common_dialect().wire_names_of(canonical).is_empty(),
            "{canonical}"
        );
    }
}

#[tokio::test]
async fn the_translated_call_equals_the_canonical_call_for_every_example() {
    let registry = presented_registry().await;
    let dialect = common_dialect();
    let mut checked = 0;
    for name in registry.names() {
        let tool = registry.get(&name).expect("registered");
        let definition = tool.definition();
        assert!(!definition.examples.is_empty(), "{name} has no example");
        for example in &definition.examples {
            let (wire_name, wire_args) = dialect
                .wire_call(&name, example)
                .unwrap_or_else(|| panic!("{name} {example}: no wire tool"));
            // The model is offered the wire tool and its schema accepts the example.
            let offered = registry
                .definitions()
                .into_iter()
                .find(|offered| offered.name == wire_name)
                .unwrap_or_else(|| panic!("{wire_name} is not offered"));
            let validator = agentkit_contracts::compile_tool_schema(&offered.input_schema)
                .expect("wire schema compiles");
            let problems: Vec<String> = validator
                .iter_errors(&wire_args)
                .map(|error| error.to_string())
                .collect();
            assert!(problems.is_empty(), "{wire_name} {wire_args}: {problems:?}");
            // And the call translates back to exactly the canonical call, which
            // the registered tool's own schema accepts.
            let translated = registry
                .translate(&wire_name, wire_args)
                .expect("translates");
            assert_eq!(translated.name, name);
            assert_eq!(&translated.args, example, "{wire_name}");
            assert!(
                registry
                    .validate_input(&translated.name, &translated.args)
                    .is_empty(),
                "{name} {example}"
            );
            checked += 1;
        }
    }
    assert!(checked >= CODING_TOOL_NAMES.len());
}

#[tokio::test]
async fn the_translated_call_gets_the_same_guard_verdict_as_the_direct_call() {
    let registry = presented_registry().await;
    let dialect = common_dialect();
    let mut compared = 0;
    for name in registry.names() {
        let tool = registry.get(&name).expect("registered");
        let definition = tool.definition().clone();
        for example in &definition.examples {
            let (wire_name, wire_args) = dialect.wire_call(&name, example).expect("wire call");
            let translated = registry
                .translate(&wire_name, wire_args)
                .expect("translates");
            // What the pipeline hands the guard: the registered tool for the
            // translated name, with the translated arguments.
            let registered = registry
                .get(&translated.name)
                .expect("translated name is registered");
            for mode in MODES {
                for role in ROLES {
                    let guard = guard(mode, role);
                    let direct = verdict(&guard, &definition, example).await;
                    let through = verdict(&guard, registered.definition(), &translated.args).await;
                    assert_eq!(direct, through, "{name} {example} {mode:?} {role:?}");
                    compared += 1;
                }
            }
        }
    }
    assert!(compared >= CODING_TOOL_NAMES.len() * MODES.len() * ROLES.len());
}

#[tokio::test]
async fn a_dialect_never_widens_authority() {
    // A wire call that the dialect cannot express as the canonical call it
    // names is refused before any guard, never run as something broader.
    let registry = presented_registry().await;
    let conflict = registry
        .translate("git_status", json!({"command": "show"}))
        .expect_err("conflicting command");
    assert!(conflict.message.contains("git_status"));
    // The write tools stay write tools whatever they are called.
    for (wire, canonical) in [
        ("edit", "edit_file"),
        ("run_shell_command", "run_command"),
        ("write_file", "write_file"),
    ] {
        let translated = registry.translate(wire, json!({})).expect("translates");
        assert_eq!(translated.name, canonical);
        let tool = registry.get(canonical).expect("registered");
        assert_ne!(tool.definition().effect, "read", "{canonical}");
    }
    // The read-only supervisor is refused for the same calls either way.
    let supervisor = guard(CodingMode::FullAuto, CodingRole::Supervisor);
    for (wire, args) in [
        (
            "edit",
            json!({"file_path": "a", "old_string": "x", "new_string": "y"}),
        ),
        ("run_shell_command", json!({"command": "ls"})),
    ] {
        let translated = registry.translate(wire, args).expect("translates");
        let registered = registry.get(&translated.name).expect("registered");
        let verdict = verdict(&supervisor, registered.definition(), &translated.args).await;
        assert!(verdict.starts_with("refused"), "{wire}: {verdict}");
    }
}

// ---- T3.2: the anthropic dialect (TU-25) -----------------------------------

/// A registry frozen the way a new Anthropic session freezes: the coding tools
/// presented as the built-in text editor and bash tools.
async fn anthropic_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in coding_tools().await {
        registry.register(tool).expect("register");
    }
    let dialect: Dialect = anthropic_dialect();
    let offered = dialect.render(&registry.definitions());
    assert!(registry.freeze_presented(offered, vec![], dialect));
    registry
}

/// The canonical call an example is expected to translate to: the editor's
/// `str_replace` always states `replace_all` false (the flat edit call's
/// default), so the translated call carries it even when the example omits it.
fn normalised(name: &str, args: &Value) -> Value {
    let mut args = args.clone();
    if name == "edit_file"
        && let Some(object) = args.as_object_mut()
    {
        object.entry("replace_all").or_insert(json!(false));
    }
    // The editor's `view` needs a path, so the dialect says the default ("."
    // = the root) out loud; the canonical call means the same.
    if name == "list_dir"
        && let Some(object) = args.as_object_mut()
    {
        object.entry("path").or_insert(json!("."));
    }
    args
}

#[tokio::test]
async fn anthropic_offers_the_two_built_in_tools_for_the_coding_pack() {
    let registry = anthropic_registry().await;
    let offered = registry.definitions();
    let builtin: Vec<(String, String)> = offered
        .iter()
        .filter_map(|definition| {
            builtin_tool_type(definition).map(|kind| (kind.to_owned(), definition.name.clone()))
        })
        .collect();
    assert_eq!(
        builtin,
        [
            (
                "text_editor_20250728".to_owned(),
                "str_replace_based_edit_tool".to_owned()
            ),
            ("bash_20250124".to_owned(), "bash".to_owned()),
        ]
    );
    // Search, git and plan tools keep the common shape.
    let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
    for common in [
        "glob",
        "grep_search",
        "git_status",
        "git_diff",
        "run_tests",
        "submit_plan",
    ] {
        assert!(names.contains(&common), "{common} in {names:?}");
    }
    for canonical in [
        "read_file",
        "write_file",
        "edit",
        "edit_file",
        "list_directory",
        "run_command",
    ] {
        assert!(
            !names.contains(&canonical),
            "{canonical} is folded into a built-in"
        );
    }
}

#[tokio::test]
async fn anthropic_translated_call_equals_the_canonical_call_for_every_expressible_example() {
    let registry = anthropic_registry().await;
    let dialect = anthropic_dialect();
    let (mut through, mut canonical_only) = (0, 0);
    for name in registry.names() {
        let definition = registry
            .get(&name)
            .expect("registered")
            .definition()
            .clone();
        for example in &definition.examples {
            let Some((wire_name, wire_args)) = dialect.wire_call(&name, example) else {
                // The built-in form cannot say it (replace_all, timeout,
                // include_ignored): shown as stored, never rewritten wider.
                assert!(
                    ["edit_file", "run_command", "list_dir"].contains(&name.as_str()),
                    "{name} {example}"
                );
                canonical_only += 1;
                continue;
            };
            let translated = registry
                .translate(&wire_name, wire_args)
                .expect("translates");
            assert_eq!(translated.name, name, "{example}");
            assert_eq!(translated.args, normalised(&name, example), "{wire_name}");
            assert!(
                registry
                    .validate_input(&translated.name, &translated.args)
                    .is_empty(),
                "{name} {example}"
            );
            through += 1;
        }
    }
    assert!(through >= CODING_TOOL_NAMES.len() - 3, "{through}");
    assert!(canonical_only >= 1);
}

#[tokio::test]
async fn anthropic_translated_call_gets_the_same_guard_verdict_as_the_direct_call() {
    let registry = anthropic_registry().await;
    let dialect = anthropic_dialect();
    let mut compared = 0;
    for name in registry.names() {
        let definition = registry
            .get(&name)
            .expect("registered")
            .definition()
            .clone();
        for example in &definition.examples {
            let Some((wire_name, wire_args)) = dialect.wire_call(&name, example) else {
                continue;
            };
            let translated = registry
                .translate(&wire_name, wire_args)
                .expect("translates");
            let registered = registry
                .get(&translated.name)
                .expect("translated name is registered");
            for mode in MODES {
                for role in ROLES {
                    let guard = guard(mode, role);
                    let direct = verdict(&guard, &definition, example).await;
                    let through = verdict(&guard, registered.definition(), &translated.args).await;
                    assert_eq!(direct, through, "{name} {example} {mode:?} {role:?}");
                    compared += 1;
                }
            }
        }
    }
    assert!(compared >= (CODING_TOOL_NAMES.len() - 3) * MODES.len() * ROLES.len());
}

#[tokio::test]
async fn anthropic_never_widens_authority() {
    let registry = anthropic_registry().await;
    let editor = "str_replace_based_edit_tool";
    // Write commands reach write tools; the read-only supervisor is refused
    // for them exactly as for the direct call.
    let supervisor = guard(CodingMode::FullAuto, CodingRole::Supervisor);
    for (wire, args, canonical) in [
        (
            editor,
            json!({"command": "str_replace", "path": "a", "old_str": "x", "new_str": "y"}),
            "edit_file",
        ),
        (
            editor,
            json!({"command": "create", "path": "a", "file_text": "x"}),
            "write_file",
        ),
        ("bash", json!({"command": "ls"}), "run_command"),
    ] {
        let translated = registry.translate(wire, args).expect("translates");
        assert_eq!(translated.name, canonical);
        let registered = registry.get(canonical).expect("registered");
        assert_ne!(registered.definition().effect, "read", "{canonical}");
        let verdict = verdict(&supervisor, registered.definition(), &translated.args).await;
        assert!(verdict.starts_with("refused"), "{canonical}: {verdict}");
    }
    // A view stays a read.
    let view = registry
        .translate(editor, json!({"command": "view", "path": "a"}))
        .expect("view");
    assert_eq!(
        registry.get(&view.name).unwrap().definition().effect,
        "read"
    );
    // Calls with no canonical equivalent are refused before any guard.
    for args in [
        json!({"command": "insert", "path": "a", "insert_line": 0, "insert_text": "x"}),
        json!({"command": "undo_edit", "path": "a"}),
    ] {
        assert!(registry.translate(editor, args).is_err());
    }
    assert!(
        registry
            .translate("bash", json!({"restart": true}))
            .is_err()
    );
}
