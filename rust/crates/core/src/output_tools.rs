//! `read_output` and `search_output` (roadmap 2.1, ADR 0034): the two direct
//! framework tools that page a stored tool result. Both have effect `read`
//! and are limited to handles of the caller's own session and its descendant
//! subagent sessions; a handle from any other session (a parent's, a
//! sibling's) answers exactly like an unknown one.
use crate::failure_catalog::{failure, failure_result};
use crate::output::{
    ByteOrLineRange, OutputError, OutputHandle, OutputStore, READ_OUTPUT, SEARCH_OUTPUT,
    SessionTree, count_lines,
};
use crate::{CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_LINES: u64 = 200;
const MAX_LINES: u64 = 500;
const DEFAULT_MATCHES: u64 = 20;
const MAX_MATCHES: u64 = 100;

pub fn read_output_definition() -> ToolDefinition {
    ToolDefinition {
        name: READ_OUTPUT.into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "output.read".into(),
        description: "Read lines of a stored tool result by its handle (out_...). Use it when a result was shown as a preview with a footer naming a handle. Lines are numbered from 1. The reply says whether more lines follow and where to continue.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "The handle from the output footer, such as out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c."},
                "start_line": {"type": "integer", "minimum": 1, "description": "First line to read. Default 1."},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_LINES, "description": "How many lines to read. Default 200."}
            },
            "required": ["handle"],
            "additionalProperties": false
        }),
        output_schema: None,
        timeout_ms: None,
        examples: vec![json!({"handle": "out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c", "start_line": 120, "max_lines": 80})],
    }
}

pub fn search_output_definition() -> ToolDefinition {
    ToolDefinition {
        name: SEARCH_OUTPUT.into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "output.search".into(),
        description: "Find lines in a stored tool result by its handle (out_...). The pattern is a regular expression, or plain text when it is not one. Each match shows its line number; read around it with read_output.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string", "description": "The handle from the output footer, such as out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c."},
                "pattern": {"type": "string", "minLength": 1, "description": "What to look for."},
                "max_matches": {"type": "integer", "minimum": 1, "maximum": MAX_MATCHES, "description": "How many matching lines to return. Default 20."}
            },
            "required": ["handle", "pattern"],
            "additionalProperties": false
        }),
        output_schema: None,
        timeout_ms: None,
        examples: vec![json!({"handle": "out_0191b2c3d4e57f8a9b0c1d2e3f4a5b6c", "pattern": "error|panicked"})],
    }
}

/// The two tools over `store`; `tree` decides which sessions' outputs a
/// caller may reach.
pub fn output_tools(store: Arc<dyn OutputStore>, tree: Arc<dyn SessionTree>) -> Vec<Arc<dyn Tool>> {
    [
        (Kind::Read, read_output_definition()),
        (Kind::Search, search_output_definition()),
    ]
    .into_iter()
    .map(|(kind, definition)| {
        Arc::new(OutputTool {
            kind,
            definition,
            store: Arc::clone(&store),
            tree: Arc::clone(&tree),
        }) as Arc<dyn Tool>
    })
    .collect()
}

#[derive(Clone, Copy)]
enum Kind {
    Read,
    Search,
}

struct OutputTool {
    kind: Kind,
    definition: ToolDefinition,
    store: Arc<dyn OutputStore>,
    tree: Arc<dyn SessionTree>,
}

fn refusal(code: &str, message: &str, limits: &ToolLimits) -> ToolResult {
    failure_result(code, &failure(code, message, None), "execution", None, limits)
}

fn unknown_handle(limits: &ToolLimits) -> ToolResult {
    refusal(
        "output_not_found",
        "No stored output has that handle in this session. Use the handle from the footer of the output exactly as written.",
        limits,
    )
}

fn ok(text: String, summary: String, limits: &ToolLimits) -> ToolResult {
    ToolResult {
        ok: true,
        data: Some(Value::String(text)),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: limits.clone(),
        model_data: None,
        summary: Some(summary),
        status: None,
        model_content: None,
    }
}

fn fault(error: &OutputError, limits: &ToolLimits) -> ToolResult {
    match error {
        OutputError::NotFound(_) | OutputError::InvalidHandle(_) => unknown_handle(limits),
        OutputError::Invalid(message) => refusal("output_invalid", message, limits),
        other => refusal("output_unavailable", &other.to_string(), limits),
    }
}

#[async_trait]
impl Tool for OutputTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let limits = context.limits.clone();
        // The same session key the output gate stores under: the scope or
        // chat, else the run itself.
        let caller = context
            .scope_id
            .as_deref()
            .or(context.chat_id.as_deref())
            .filter(|session| !session.is_empty())
            .unwrap_or(context.run_id.as_str());
        if caller.is_empty() {
            return Ok(unknown_handle(&limits));
        }
        let Ok(handle) = OutputHandle::parse(input["handle"].as_str().unwrap_or_default()) else {
            return Ok(unknown_handle(&limits));
        };
        let info = match self.store.info(&handle).await {
            Ok(info) => info,
            Err(error) => return Ok(fault(&error, &limits)),
        };
        if info.meta.session != caller && !self.tree.may_read(caller, &info.meta.session).await {
            return Ok(unknown_handle(&limits));
        }
        Ok(match self.kind {
            Kind::Read => self.read(&handle, &input, &limits).await,
            Kind::Search => self.search(&handle, &input, &limits).await,
        })
    }
}

impl OutputTool {
    async fn read(&self, handle: &OutputHandle, input: &Value, limits: &ToolLimits) -> ToolResult {
        let start_line = input["start_line"].as_u64().unwrap_or(1).max(1);
        let max_lines = input["max_lines"]
            .as_u64()
            .unwrap_or(DEFAULT_LINES)
            .clamp(1, MAX_LINES);
        let slice = match self
            .store
            .read(
                handle,
                ByteOrLineRange::Lines {
                    start_line,
                    max_lines,
                },
            )
            .await
        {
            Ok(slice) => slice,
            Err(error) => return fault(&error, limits),
        };
        let cap = (limits.max_bytes / 4 * 3).clamp(1024, 12 * 1024);
        let mut text = slice.text;
        let mut end_line = slice.end_line;
        let mut has_more = slice.has_more;
        let mut line_cut = false;
        if text.len() > cap {
            has_more = true;
            match text[..floor_boundary(&text, cap)].rfind('\n') {
                Some(at) => text.truncate(at),
                None => {
                    text.truncate(floor_boundary(&text, cap));
                    line_cut = true;
                }
            }
            end_line = start_line + count_lines(text.as_bytes()).saturating_sub(1);
        }
        let mut footer = if slice.end_line < start_line {
            format!(
                "[no lines: the output has {} lines]",
                slice.total_lines
            )
        } else {
            format!(
                "[lines {start_line}-{end_line} of {}{}]",
                slice.total_lines,
                if has_more {
                    format!(
                        " · more: read_output(handle, start_line={}, max_lines)",
                        end_line + 1
                    )
                } else {
                    String::new()
                }
            )
        };
        if line_cut {
            footer.push_str(" [line cut at the size limit]");
        }
        let summary = format!("Read lines {start_line}-{end_line} of {}.", slice.total_lines);
        ok(format!("{text}\n{footer}"), summary, limits)
    }

    async fn search(&self, handle: &OutputHandle, input: &Value, limits: &ToolLimits) -> ToolResult {
        let pattern = input["pattern"].as_str().unwrap_or_default();
        let max = input["max_matches"]
            .as_u64()
            .unwrap_or(DEFAULT_MATCHES)
            .clamp(1, MAX_MATCHES) as u32;
        let matches = match self.store.search(handle, pattern, max).await {
            Ok(matches) => matches,
            Err(error) => return fault(&error, limits),
        };
        if matches.is_empty() {
            return ok(
                "[no matching lines]".into(),
                format!("No line matches \"{pattern}\"."),
                limits,
            );
        }
        let mut text = String::new();
        for found in &matches {
            text.push_str(&format!("{}: {}\n", found.line, found.text));
        }
        let full = matches.len() as u32 >= max;
        text.push_str(&format!(
            "[{} matching lines{}]",
            matches.len(),
            if full {
                " · more may follow: narrow the pattern or raise max_matches"
            } else {
                ""
            }
        ));
        ok(
            text,
            format!("{} lines match \"{pattern}\".", matches.len()),
            limits,
        )
    }
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}
