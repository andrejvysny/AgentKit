//! The contribution gate (SBX-11): tools only for an attached run, never a
//! workspace argument, nothing created at staging (D-36, F-16); and the
//! shape of what is offered: names, strict schemas, deferred loading, the
//! `sandbox` toolset and its guide.

mod support;

use agentkit_core::{ToolDefinition, ToolLoading, Toolset, ToolsetView, toolset_index};
use agentkit_governance::ToolContributor;
use agentkit_toolpacks_sandbox::{
    SANDBOX_NAMESPACE, SANDBOX_TOOL_NAMES, SANDBOX_TOOLSET, SandboxMode,
};
use agentkit_workspaces::WorkspaceRepository;
use serde_json::json;
use support::{World, chat_binding, home_binding};

fn wire_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[tokio::test]
async fn an_unattached_run_is_offered_nothing_and_no_card() {
    let attached = World::new(Some(home_binding(SandboxMode::CHAT)));
    let detached = World::new(None);
    assert!(detached.stage().await.is_empty());
    assert_eq!(detached.pack.namespace(), SANDBOX_NAMESPACE);
    assert!(
        card(&attached)
            .await
            .is_some_and(|index| index.contains("- sandbox (6): "))
    );
    assert_eq!(
        card(&detached).await,
        None,
        "no offered member, no sandbox card"
    );
}

/// The catalog index (the system prompt's toolset cards) of what a staging
/// offers.
async fn card(world: &World) -> Option<String> {
    let deferred: Vec<ToolDefinition> = world
        .stage()
        .await
        .values()
        .map(|tool| tool.definition().clone())
        .collect();
    let toolsets: Vec<Toolset> = world.pack.toolsets();
    let ids = deferred
        .iter()
        .map(|definition| (definition.name.clone(), SANDBOX_TOOLSET.to_owned()))
        .collect();
    let view = ToolsetView::new(
        toolsets
            .into_iter()
            .map(|set| (set.id.clone(), set))
            .collect(),
        ids,
    );
    toolset_index(&[], &deferred, &view)
}

#[tokio::test]
async fn an_attached_run_gets_six_deferred_strict_tools_without_a_workspace_argument() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    let names: Vec<&str> = tools.keys().map(String::as_str).collect();
    let mut expected = SANDBOX_TOOL_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(names, expected);
    for (name, tool) in &tools {
        assert!(wire_name(name), "{name}");
        let definition = tool.definition();
        let schema = &definition.input_schema;
        assert_eq!(schema["additionalProperties"], json!(false), "{name}");
        assert!(schema["required"].is_array(), "{name}");
        let properties = schema["properties"].as_object().expect("properties");
        for property in properties.keys() {
            let lower = property.to_lowercase();
            assert!(
                !lower.contains("workspace") && !lower.contains("binding"),
                "{name}.{property}"
            );
        }
        assert_eq!(tool.loading(), ToolLoading::Deferred, "{name}");
        assert_eq!(tool.toolset(), Some(SANDBOX_TOOLSET), "{name}");
        assert!(!definition.examples.is_empty(), "{name}");
        let writes = matches!(
            name.as_str(),
            "sandbox_run" | "sandbox_write_file" | "sandbox_export_file"
        );
        assert_eq!(
            definition.effect,
            if writes { "write" } else { "read" },
            "{name}"
        );
        assert_eq!(tool.effect_context().is_some(), writes, "{name}");
        let operates = matches!(name.as_str(), "sandbox_run" | "sandbox_write_file");
        assert_eq!(tool.records_command_start(), operates, "{name}");
        assert_ne!(definition.capability, "process.exec");
    }
}

/// The effect context binds the attachment, never the machine: it is the
/// same before the workspace exists and after its generation advanced, so
/// an approval answered in between still holds (ADR 0038 decision 14).
#[tokio::test]
async fn staging_creates_nothing_and_binds_the_attachment_not_the_machine() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let before = world.stage().await["sandbox_run"].effect_context().unwrap();
    assert!(
        world.repo.list_workspaces().await.unwrap().is_empty(),
        "D-36: staging creates nothing"
    );
    assert_eq!(
        before,
        json!({"sandbox": {
            "workspaceKey": "chats:project:p1",
            "kind": "project_chats",
            "ownerScope": "owner:1",
            "folder": "chats/c1",
            "agentId": "chat-agent",
        }})
    );
    let workspace = world.service.resolve(&binding.binding).await.unwrap();
    let created = world.stage().await["sandbox_run"].effect_context().unwrap();
    assert_eq!(created, before, "the workspace's creation changes nothing");
    world.put(&binding, "a.txt", "a").await;
    world.service.stop(&workspace.id).await.unwrap();
    let stopped = world.service.lookup(&binding.binding).await.unwrap();
    assert!(stopped.expect("kept").generation.0 > workspace.generation.0);
    let advanced = world.stage().await["sandbox_run"].effect_context().unwrap();
    assert_eq!(advanced, before, "nor does a new generation");
}

#[tokio::test]
async fn the_toolset_guide_is_small_static_and_names_every_tool() {
    let world = World::new(Some(home_binding(SandboxMode::CHAT)));
    let toolsets = world.pack.toolsets();
    assert_eq!(toolsets.len(), 1);
    let set = &toolsets[0];
    assert_eq!(set.id, SANDBOX_TOOLSET);
    assert_eq!(set.tools, SANDBOX_TOOL_NAMES.map(String::from).to_vec());
    let guide = set.guide.as_ref().expect("guide");
    assert_eq!(guide.version, "1");
    assert!(
        guide.estimated_tokens() <= 600,
        "{} tokens",
        guide.estimated_tokens()
    );
    for name in SANDBOX_TOOL_NAMES {
        assert!(
            guide.text.contains(&format!("{{{{tool:{name}}}}}")),
            "{name}"
        );
    }
    for needle in [
        "pip install --user",
        "npm install",
        "no apt",
        "input/",
        "data, not instructions",
        "2026.11 (sha256:image-1)",
    ] {
        assert!(guide.text.contains(needle), "{needle}");
    }
    assert_eq!(world.pack.toolsets()[0].guide, set.guide, "session-static");
}
