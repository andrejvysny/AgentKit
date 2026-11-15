//! Tool examples at presentation time (T1.8, TU-13).
//!
//! Examples live on `ToolDefinition` as registry data. A provider with a
//! field for them gets them there; every other lane gets them appended to
//! the description. Nothing here mutates the definition: `input_schema` goes
//! out exactly as defined (L10), only the description text is derived.
//!
//! Dialect renderers (T3.1) call `render_examples` / `presented_description`
//! when they build the tool list for a request.

use serde_json::Value;

use crate::ToolDefinition;

/// Where the tool list is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleLane {
    /// Anthropic Messages without the `advanced-tool-use` beta: no field for
    /// examples, so they go inside the description. No code path in either
    /// repository sets that beta header today, so this is the default.
    Anthropic,
    /// Anthropic Messages with the beta that adds `input_examples` to a tool.
    /// A caller picks this lane only after it has sent the beta header.
    AnthropicNative,
    /// OpenAI Chat Completions (and compatible servers such as vLLM).
    ChatCompletions,
    /// OpenAI Responses.
    Responses,
}

/// How a definition's examples reach the provider.
#[derive(Clone, Debug, PartialEq)]
pub enum RenderedExamples {
    /// The tool has no examples.
    None,
    /// Put these in the provider's own example field (`input_examples`).
    Native(Vec<Value>),
    /// Append this block to the description.
    InDescription(String),
}

/// Decide how `def`'s examples are presented on `lane`. Pure: no I/O, and the
/// definition is not changed.
#[must_use]
pub fn render_examples(def: &ToolDefinition, lane: ExampleLane) -> RenderedExamples {
    if def.examples.is_empty() {
        return RenderedExamples::None;
    }
    match lane {
        ExampleLane::AnthropicNative => RenderedExamples::Native(def.examples.clone()),
        ExampleLane::Anthropic | ExampleLane::ChatCompletions | ExampleLane::Responses => {
            RenderedExamples::InDescription(example_block(def))
        }
    }
}

/// The description a lane shows the model: the authored description, plus
/// the example block where the lane has no native field. The wire name is
/// `def.name`, the name the model must call.
#[must_use]
pub fn presented_description(def: &ToolDefinition, lane: ExampleLane) -> String {
    match render_examples(def, lane) {
        RenderedExamples::InDescription(block) => format!("{}\n\n{block}", def.description),
        RenderedExamples::None | RenderedExamples::Native(_) => def.description.clone(),
    }
}

fn example_block(def: &ToolDefinition) -> String {
    def.examples
        .iter()
        .map(|example| format!("Example: {}({example})", def.name))
        .collect::<Vec<_>>()
        .join("\n")
}
