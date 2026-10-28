//! Port of `packages/toolpacks-coding/tests/contributor.test.ts`: the pack's
//! registry surface — names, order, effects, capabilities, schemas.

use std::sync::Arc;

use agentkit_core::{ToolLimits, ToolRegistry};
use agentkit_host::{ContributedTool, ToolContributionContext};
use agentkit_toolpacks_coding::{
    CODING_NAMESPACE, CODING_TOOL_NAMES, CodingLimitsOverride, CodingToolPackOptions,
    CodingWorkspace, SandboxMode, create_coding_tool_pack,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn contribution() -> ToolContributionContext {
    ToolContributionContext {
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

async fn pack() -> Arc<agentkit_toolpacks_coding::CodingToolPack> {
    create_coding_tool_pack(CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: "/tmp".into(),
        },
        ..test_options()
    })
    .await
    .unwrap()
}

fn test_options() -> CodingToolPackOptions {
    CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: "/tmp".into(),
        },
        limits: CodingLimitsOverride::default(),
        allow_sensitive: false,
        clock: None,
        sandbox: SandboxMode::Off,
        network: false,
        allow_git_writes: false,
        test_commands: vec![],
        platform: Some("linux".into()),
        sandbox_exec_path: None,
        coordinator: None,
        coordination_wait_ms: None,
        permission: None,
    }
}

/// `[name, effect, capability, definition timeout]` — the TS definitions'
/// own numbers (fs/exec/plan: `toolTimeoutMs` 30 000; search:
/// `searchTimeBudgetMs + 5000`; git: `gitTimeoutMs + 5000`; commands:
/// `(commandMaxTimeoutSec + 30) * 1000`).
const EXPECTED: &[(&str, &str, &str, u64)] = &[
    ("read_file", "read", "fs.read", 30_000),
    ("list_dir", "read", "fs.read", 30_000),
    ("find_files", "read", "fs.read", 25_000),
    ("code_search", "read", "fs.read", 25_000),
    ("repo_map", "read", "fs.read", 25_000),
    ("git_inspect", "read", "git.read", 25_000),
    ("edit_file", "write", "fs.write", 30_000),
    ("write_file", "write", "fs.write", 30_000),
    ("run_command", "write", "process.exec", 630_000),
    ("run_tests", "write", "process.exec", 630_000),
    ("submit_plan", "read", "plan", 30_000),
];

#[tokio::test]
async fn registers_eleven_tools_under_namespace_coding() {
    let pack = pack().await;
    use agentkit_host::ToolContributor;
    assert_eq!(pack.namespace(), CODING_NAMESPACE);
    assert_eq!(CODING_NAMESPACE, "coding");
    let tools = pack.contribute(&contribution()).await.unwrap();
    assert_eq!(tools.len(), 11);
    let names: Vec<String> = tools
        .iter()
        .map(|tool| match tool {
            ContributedTool::Read(t) | ContributedTool::DirectWrite(t) => {
                t.definition().name.clone()
            }
            ContributedTool::Write(_) => panic!("coding tools must not be proposal-governed"),
        })
        .collect();
    assert_eq!(names, CODING_TOOL_NAMES.to_vec());
}

#[tokio::test]
async fn effects_capabilities_and_schemas() {
    use agentkit_host::ToolContributor;
    let tools = pack().await.contribute(&contribution()).await.unwrap();
    let mut registry = ToolRegistry::new();
    for (tool, (name, effect, capability, timeout_ms)) in tools.iter().zip(EXPECTED) {
        let definition = match tool {
            ContributedTool::Read(t) | ContributedTool::DirectWrite(t) => t,
            ContributedTool::Write(_) => panic!("{name} must not be proposal-governed"),
        };
        let definition = definition.definition();
        assert_eq!(definition.name, *name);
        assert_eq!(definition.effect, *effect, "{name} effect");
        assert_eq!(definition.capability, *capability, "{name} capability");
        assert_eq!(definition.timeout_ms, Some(*timeout_ms), "{name} timeout");
        assert!(!definition.description.is_empty(), "{name} description");
        let schema = &definition.input_schema;
        assert_eq!(schema["type"], "object", "{name} schema type");
        assert_eq!(
            schema["additionalProperties"], false,
            "{name} must be strict"
        );
        registry
            .register(match tool {
                ContributedTool::Read(t) | ContributedTool::DirectWrite(t) => t.clone(),
                ContributedTool::Write(_) => unreachable!(),
            })
            .unwrap();
    }
    assert_eq!(registry.len(), 11);

    // Schema enforcement mirrors the TS assertions.
    assert!(
        registry
            .validate_input("submit_plan", &json!({ "plan": "do it" }))
            .is_empty()
    );
    assert!(
        !registry
            .validate_input("submit_plan", &json!({ "plan": "do it", "extra": 1 }))
            .is_empty()
    );
    assert!(
        !registry
            .validate_input("submit_plan", &json!({}))
            .is_empty()
    );
    assert!(
        registry
            .validate_input("read_file", &json!({ "path": "a.ts" }))
            .is_empty()
    );
    assert!(
        !registry
            .validate_input("read_file", &json!({ "path": "a.ts", "x": 1 }))
            .is_empty()
    );
    assert!(!registry.validate_input("read_file", &json!({})).is_empty());
    assert!(
        !registry
            .validate_input("git_inspect", &json!({ "command": "push" }))
            .is_empty()
    );
    assert!(
        !registry
            .validate_input(
                "run_command",
                &json!({ "command": "ls", "timeoutSec": 601 })
            )
            .is_empty()
    );
    assert!(
        !registry
            .validate_input("edit_file", &json!({ "path": "a", "edits": [] }))
            .is_empty()
    );
    assert!(
        !registry
            .validate_input("code_search", &json!({ "pattern": "x", "context": 21 }))
            .is_empty()
    );
}

#[tokio::test]
async fn rejects_bad_configuration() {
    // Relative root.
    let options = CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: "relative/path".into(),
        },
        ..test_options()
    };
    let error = create_coding_tool_pack(options)
        .await
        .map(|_| ())
        .expect_err("relative root must fail");
    assert_eq!(error.message, "workspace.root must be an absolute path");

    // Zero limit.
    let limits = CodingLimitsOverride {
        read_max_lines: Some(0),
        ..CodingLimitsOverride::default()
    };
    let options = CodingToolPackOptions {
        limits,
        ..test_options()
    };
    let error = create_coding_tool_pack(options)
        .await
        .map(|_| ())
        .expect_err("zero limit must fail");
    assert_eq!(
        error.message,
        "coding limit \"readMaxLines\" must be a positive number"
    );

    // coordinationWaitMs at or above toolTimeoutMs.
    let options = CodingToolPackOptions {
        coordination_wait_ms: Some(30_000),
        ..test_options()
    };
    let error = create_coding_tool_pack(options)
        .await
        .map(|_| ())
        .expect_err("wait cap must fail");
    assert_eq!(
        error.message,
        "coordinationWaitMs must be positive and below the toolTimeoutMs limit"
    );
}
