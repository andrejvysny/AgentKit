//! TU-23 and TU-26 for the OpenAI dialect: the vendor patch and shell calls
//! translate to the canonical call the direct call would be, and the coding
//! permission guard gives the same verdict for both, in every mode and role.
//! A call the canonical tools cannot express is refused before any guard.
//!
//! The guard sees the call the call pipeline hands it: the translated one,
//! with the registered (canonical) tool definition.

use std::sync::Arc;

use agentkit_core::{
    CancellationToken, Tool, ToolDefinition, ToolLimits, ToolRegistry, openai_dialect,
};
use agentkit_governance::{
    ContributedTool, ToolContributionContext, ToolContributor, ToolGuard, ToolGuardContext,
    ToolGuardVerdict,
};
use agentkit_toolpacks_coding::{
    CODING_NAMESPACE, CodingLimitsOverride, CodingMode, CodingPermissionGuard,
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
    let dialect = openai_dialect();
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
            format!("awaiting: {} / {:?} / {:?}", ask.reason, ask.preview, ask.requested_mode)
        }
    }
}

async fn verdict(
    guard: &CodingPermissionGuard,
    tool: &ToolDefinition,
    args: &Value,
) -> String {
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


fn vendor_fixture() -> Value {
    serde_json::from_str(include_str!("../../core/tests/fixtures/dialect-openai-v1.json"))
        .expect("fixture")
}

#[tokio::test]
async fn the_coding_pack_is_offered_as_patch_shell_and_strict_common_tools() {
    let registry = presented_registry().await;
    let offered = registry.definitions();
    let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"apply_patch") && names.contains(&"shell"), "{names:?}");
    for gone in ["write_file", "edit", "run_shell_command", "edit_file", "run_command"] {
        assert!(!names.contains(&gone), "{gone} is offered next to the built-ins");
    }
    // Most of the other coding tools have a schema strict mode takes.
    let strict = offered
        .iter()
        .filter(|d| agentkit_core::is_strict_compliant(&d.input_schema))
        .count();
    assert!(strict >= 3, "{strict} strict of {names:?}");
}

#[tokio::test]
async fn every_example_with_a_vendor_form_translates_back_to_itself() {
    let registry = presented_registry().await;
    let dialect = openai_dialect();
    let mut with_form = vec![];
    for name in registry.names() {
        let tool = registry.get(&name).expect("registered");
        for example in &tool.definition().examples {
            let Some((wire_name, wire_args)) = dialect.wire_call(&name, example) else {
                continue;
            };
            // Strict tools show every property; built-ins show the vendor payload.
            let translated = registry.translate(&wire_name, wire_args).expect("translates");
            assert_eq!(translated.name, name);
            assert_eq!(&translated.args, example, "{name}");
            assert!(registry.validate_input(&name, &translated.args).is_empty(), "{name} {example}");
            with_form.push(name.clone());
        }
    }
    for expected in ["write_file", "edit_file", "run_command", "read_file"] {
        assert!(with_form.iter().any(|name| name == expected), "{expected}: {with_form:?}");
    }
}

#[tokio::test]
async fn a_strict_call_with_null_for_each_optional_argument_equals_the_canonical_example() {
    let registry = presented_registry().await;
    let dialect = openai_dialect();
    let offered = registry.definitions();
    let mut checked = 0;
    for name in registry.names() {
        if matches!(name.as_str(), "write_file" | "edit_file" | "run_command") {
            continue;
        }
        let tool = registry.get(&name).expect("registered");
        for example in &tool.definition().examples {
            let Some((wire_name, mut wire_args)) = dialect.wire_call(&name, example) else {
                continue;
            };
            let shown = offered.iter().find(|d| d.name == wire_name).expect("offered");
            let properties = shown.input_schema["properties"].as_object().expect("properties");
            for key in properties.keys() {
                wire_args
                    .as_object_mut()
                    .expect("object")
                    .entry(key.clone())
                    .or_insert(Value::Null);
            }
            let translated = registry.translate(&wire_name, wire_args).expect("translates");
            assert_eq!(translated.name, name);
            assert_eq!(&translated.args, example, "{name}");
            checked += 1;
        }
    }
    assert!(checked >= 3, "{checked}");
}

#[tokio::test]
async fn the_vendor_call_gets_the_same_guard_verdict_as_the_direct_call() {
    let registry = presented_registry().await;
    let mut compared = 0;
    for pair in vendor_fixture()["response"]["calls"].as_array().expect("calls") {
        let wire = &pair["wire"];
        let translated = registry
            .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
            .expect("translates");
        assert_eq!(translated.name, pair["canonical"]["name"]);
        assert_eq!(translated.args, pair["canonical"]["arguments"]);
        let registered = registry.get(&translated.name).expect("registered");
        let direct = registered.definition().clone();
        for mode in MODES {
            for role in ROLES {
                let guard = guard(mode, role);
                let a = verdict(&guard, &direct, &pair["canonical"]["arguments"]).await;
                let b = verdict(&guard, registered.definition(), &translated.args).await;
                assert_eq!(a, b, "{wire} {mode:?} {role:?}");
                compared += 1;
            }
        }
    }
    assert!(compared >= 5 * MODES.len() * ROLES.len());
}

#[tokio::test]
async fn the_vendor_tools_never_widen_authority() {
    let registry = presented_registry().await;
    // The patch and shell tools are write tools whatever they are called.
    for (wire, args, canonical) in [
        ("apply_patch", json!({"type":"create_file","path":"a","diff":"+x"}), "write_file"),
        ("apply_patch", json!({"type":"update_file","path":"a","diff":"@@\n-x\n+y\n"}), "edit_file"),
        ("shell", json!({"commands":["ls"]}), "run_command"),
    ] {
        let translated = registry.translate(wire, args).expect("translates");
        assert_eq!(translated.name, canonical);
        let tool = registry.get(canonical).expect("registered");
        assert_ne!(tool.definition().effect, "read", "{canonical}");
    }
    // The read-only supervisor is refused for the same calls either way.
    let supervisor = guard(CodingMode::FullAuto, CodingRole::Supervisor);
    for (wire, args) in [
        ("apply_patch", json!({"type":"update_file","path":"a","diff":"@@\n-x\n+y\n"})),
        ("apply_patch", json!({"type":"create_file","path":"a","diff":"+x"})),
        ("shell", json!({"commands":["ls"]})),
    ] {
        let translated = registry.translate(wire, args).expect("translates");
        let registered = registry.get(&translated.name).expect("registered");
        let verdict = verdict(&supervisor, registered.definition(), &translated.args).await;
        assert!(verdict.starts_with("refused"), "{wire}: {verdict}");
    }
    // What the canonical tools cannot do is refused before any guard.
    for case in vendor_fixture()["response"]["refused"].as_array().expect("refused") {
        let wire = &case["wire"];
        assert!(
            registry
                .translate(wire["name"].as_str().unwrap(), wire["arguments"].clone())
                .is_err(),
            "{wire}"
        );
    }
}
