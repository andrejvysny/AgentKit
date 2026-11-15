//! TU-13: examples are registry data, rendered natively where the provider
//! has a field and inside the description elsewhere. Per-lane snapshots.

use agentkit_core::{ExampleLane, RenderedExamples, ToolDefinition, presented_description, render_examples};
use serde_json::json;

fn tool(examples: serde_json::Value) -> ToolDefinition {
    serde_json::from_value(json!({
        "name": "read_file",
        "version": "1",
        "effect": "read",
        "capability": "fs",
        "description": "Read one file. Returns its lines.",
        "inputSchema": {"type": "object", "properties": {"file_path": {"type": "string"}}},
        "examples": examples,
    }))
    .unwrap()
}

const SNAPSHOT: &str = "Read one file. Returns its lines.\n\n\
Example: read_file({\"file_path\":\"src/main.rs\"})\n\
Example: read_file({\"file_path\":\"README.md\",\"offset\":10})";

fn two() -> ToolDefinition {
    tool(json!([
        {"file_path": "src/main.rs"},
        {"file_path": "README.md", "offset": 10},
    ]))
}

#[test]
fn chat_completions_responses_and_plain_anthropic_append_the_block() {
    let def = two();
    for lane in [
        ExampleLane::ChatCompletions,
        ExampleLane::Responses,
        ExampleLane::Anthropic,
    ] {
        assert_eq!(presented_description(&def, lane), SNAPSHOT, "{lane:?}");
        assert!(matches!(
            render_examples(&def, lane),
            RenderedExamples::InDescription(_)
        ));
    }
    // Rendered length is recorded in the snapshot: authored text plus the block.
    assert_eq!(SNAPSHOT.chars().count(), 140);
}

#[test]
fn anthropic_native_lane_uses_the_provider_field_and_keeps_the_description() {
    let def = two();
    assert_eq!(
        render_examples(&def, ExampleLane::AnthropicNative),
        RenderedExamples::Native(def.examples.clone())
    );
    assert_eq!(
        presented_description(&def, ExampleLane::AnthropicNative),
        def.description
    );
}

#[test]
fn a_tool_without_examples_is_presented_unchanged() {
    let def = tool(json!([]));
    for lane in [
        ExampleLane::Anthropic,
        ExampleLane::AnthropicNative,
        ExampleLane::ChatCompletions,
        ExampleLane::Responses,
    ] {
        assert_eq!(render_examples(&def, lane), RenderedExamples::None);
        assert_eq!(presented_description(&def, lane), def.description);
    }
}

#[test]
fn rendering_does_not_touch_the_defined_tool() {
    let def = two();
    let before = serde_json::to_value(&def).unwrap();
    let _ = presented_description(&def, ExampleLane::ChatCompletions);
    let _ = render_examples(&def, ExampleLane::AnthropicNative);
    assert_eq!(serde_json::to_value(&def).unwrap(), before);
}
