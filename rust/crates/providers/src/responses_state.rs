use crate::{responses_request::bounded, transport::classify};
use agentkit_core::{CoreError, ProviderEvent, ToolCall, ToolDefinition};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
struct Item {
    id: String,
    index: usize,
    item: Value,
    arguments: String,
    texts: BTreeMap<usize, String>,
    done: bool,
}
#[derive(Default)]
pub(crate) struct ResponsesState {
    items: BTreeMap<usize, Item>,
    terminal: Option<Value>,
    emitted: String,
    bytes: usize,
    pub usage: Option<Value>,
    /// Per-part reasoning text keyed `item_id:summary|content:index`
    /// (mirrors `ResponsesReasoning` in the TypeScript reference client).
    /// A Vec, not a map: the frozen reference concatenates with `Map`
    /// ITERATION ORDER (first appearance), so the pairing must preserve
    /// arrival order exactly.
    reasoning_parts: Vec<(String, String)>,
    reasoning_pending: String,
    /// Function call progress from the last accepted frame (output index).
    /// The run loop throttles what it persists.
    tool_progress: Option<usize>,
}
pub(crate) struct ResponsesResult {
    pub output: Vec<Value>,
    pub content: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Value>,
    /// The frozen `reasoningContent`: every reasoning part the stream
    /// revealed, terminal-reconciled (final text must extend the streamed
    /// prefix or the run fails `stream_output_mismatch`; reasoning that
    /// only the terminal response carries is preserved too).
    pub reasoning: String,
}
impl ResponsesState {
    fn reasoning_part(&self, key: &str) -> Option<&str> {
        self.reasoning_parts
            .iter()
            .find(|(part_key, _)| part_key == key)
            .map(|(_, text)| text.as_str())
    }
    /// `Map.set` semantics: update in place (keeping the original insertion
    /// slot) or append for a first-time key.
    fn set_reasoning_part(&mut self, key: String, text: String) {
        match self
            .reasoning_parts
            .iter_mut()
            .find(|(part_key, _)| part_key == &key)
        {
            Some(entry) => entry.1 = text,
            None => self.reasoning_parts.push((key, text)),
        }
    }
    pub fn accept(&mut self, event: Value) -> Result<Option<String>, CoreError> {
        self.bytes += bounded(&event)?.len();
        if self.bytes > 16 * 1024 * 1024 {
            return err("stream_limit_exceeded");
        }
        let kind = string(&event["type"])?;
        if [
            "response.completed",
            "response.failed",
            "response.incomplete",
            "response.in_progress",
        ]
        .contains(&kind)
            && event["response"]["usage"].is_object()
        {
            self.usage = Some(event["response"]["usage"].clone());
        }
        if ["response.failed", "error"].contains(&kind) {
            return err(classify(
                if kind == "error" {
                    event["code"].as_str()
                } else {
                    event["response"]["error"]["code"].as_str()
                }
                .unwrap_or(""),
            ));
        }
        if kind == "response.incomplete" {
            return err("response_incomplete");
        }
        if self.terminal.is_some() {
            return err("event_after_terminal");
        }
        match kind {
            "response.completed" => {
                let response = &event["response"];
                if response["status"] != "completed"
                    || truthy(&response["error"])
                    || truthy(&response["incomplete_details"])
                {
                    return err("response_incomplete");
                }
                self.terminal = Some(response.clone());
            }
            "response.output_item.added" => self.add(&event)?,
            "response.output_item.done" => {
                let mut lookup = event.clone();
                lookup["item_id"] = event["item"]["id"].clone();
                let item = self.lookup_mut(&lookup)?;
                if item.done || item.item["type"] != event["item"]["type"] {
                    return err("output_item_mismatch");
                }
                compare(item, &event["item"])?;
                item.item = event["item"].clone();
                item.done = true;
            }
            "response.function_call_arguments.delta" => {
                let item = self.lookup_mut(&event)?;
                if item.item["type"] != "function_call" || item.done {
                    return err("malformed_response");
                }
                item.arguments.push_str(string(&event["delta"])?);
                bounded(&Value::String(item.arguments.clone()))?;
                self.tool_progress = Some(item.index);
            }
            "response.function_call_arguments.done" => {
                let item = self.lookup_mut(&event)?;
                let arguments = string(&event["arguments"])?;
                if item.item["type"] != "function_call"
                    || item.done
                    || !arguments.starts_with(&item.arguments)
                {
                    return err("stream_output_mismatch");
                }
                item.arguments = arguments.into();
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let item = self.lookup_mut(&event)?;
                if item.item["type"] != "message" || item.done {
                    return err("malformed_response");
                }
                let index = index(&event["content_index"])?;
                let delta = string(&event["delta"])?;
                item.texts.entry(index).or_default().push_str(delta);
                self.emitted.push_str(delta);
                bounded(&Value::String(self.emitted.clone()))?;
                return Ok(Some(delta.into()));
            }
            "response.output_text.done" | "response.refusal.done" => {
                let item = self.lookup_mut(&event)?;
                let index = index(&event["content_index"])?;
                let text = string(if kind == "response.refusal.done" {
                    &event["refusal"]
                } else {
                    &event["text"]
                })?;
                if !text.starts_with(item.texts.get(&index).map(String::as_str).unwrap_or("")) {
                    return err("stream_output_mismatch");
                }
                item.texts.insert(index, text.into());
            }
            _ => self.auxiliary(&event, kind)?,
        }
        Ok(None)
    }
    fn add(&mut self, event: &Value) -> Result<(), CoreError> {
        let item = event["item"].clone();
        let index = index(&event["output_index"])?;
        let id = nonempty(&item["id"])?;
        if !["message", "function_call", "reasoning"].contains(&string(&item["type"])?) {
            return err("unsupported_output");
        }
        if self.items.contains_key(&index)
            || self.items.values().any(|item| item.id == id)
            || self.items.len() >= 1024
        {
            return err("duplicate_output_item");
        }
        let function_call = item["type"] == "function_call";
        self.items.insert(
            index,
            Item {
                id: id.into(),
                index,
                item,
                arguments: String::new(),
                texts: BTreeMap::new(),
                done: false,
            },
        );
        if function_call {
            self.tool_progress = Some(index);
        }
        Ok(())
    }
    /// `ToolCallProgress` for the function call the last accepted frame
    /// started or extended. Its id is the `call_id` the finished call carries.
    pub fn take_tool_progress(&mut self) -> Option<ProviderEvent> {
        let item = self.items.get(&self.tool_progress.take()?)?;
        let name = item.item["name"].as_str().filter(|name| !name.is_empty())?;
        Some(ProviderEvent::ToolCallProgress {
            id: item.item["call_id"].as_str().unwrap_or(&item.id).into(),
            name: name.into(),
            args_bytes: item.arguments.len() as u64,
        })
    }
    fn lookup_mut(&mut self, event: &Value) -> Result<&mut Item, CoreError> {
        let id = nonempty(&event["item_id"])?;
        let index = index(&event["output_index"])?;
        let item = self
            .items
            .get_mut(&index)
            .ok_or_else(|| CoreError::responses("output_item_mismatch"))?;
        if item.id != id || item.index != index {
            return err("output_item_mismatch");
        }
        Ok(item)
    }
    fn auxiliary(&mut self, event: &Value, kind: &str) -> Result<(), CoreError> {
        if ["response.created", "response.in_progress"].contains(&kind) {
            nonempty(&event["response"]["id"])?;
            if event["response"]["status"] != "in_progress" {
                return err("malformed_response");
            }
            return Ok(());
        }
        if ["response.content_part.added", "response.content_part.done"].contains(&kind) {
            let item = self.lookup_mut(event)?;
            if item.item["type"] != "message" || item.done {
                return err("output_item_mismatch");
            }
            index(&event["content_index"])?;
            if !["output_text", "refusal"].contains(&string(&event["part"]["type"])?) {
                return err("unsupported_output");
            }
            return Ok(());
        }
        const REASONING: &[&str] = &[
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_part.done",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_text.delta",
            "response.reasoning_text.done",
        ];
        if !REASONING.contains(&kind) {
            return err("unsupported_stream_event");
        }
        let item = self.lookup_mut(event)?;
        if item.item["type"] != "reasoning" || item.done {
            return err("output_item_mismatch");
        }
        let part_index = index(if kind.contains("summary") {
            &event["summary_index"]
        } else {
            &event["content_index"]
        })?;
        let key = format!(
            "{}:{}:{}",
            string(&event["item_id"])?,
            if kind.contains("summary") {
                "summary"
            } else {
                "content"
            },
            part_index
        );
        if kind.contains("_part.") {
            if event["part"]["type"] != "summary_text" {
                return err("malformed_response");
            }
            let text = string(&event["part"]["text"])?;
            self.commit_reasoning(key, text.to_owned())?;
        } else if kind.ends_with(".delta") {
            let delta = string(&event["delta"])?.to_owned();
            match self
                .reasoning_parts
                .iter_mut()
                .find(|(part_key, _)| part_key == &key)
            {
                Some(entry) => entry.1.push_str(&delta),
                None => self.reasoning_parts.push((key, delta.clone())),
            }
            self.reasoning_pending.push_str(&delta);
        } else {
            let text = string(&event["text"])?.to_owned();
            self.commit_reasoning(key, text)?;
        }
        Ok(())
    }
    fn commit_reasoning(&mut self, key: String, text: String) -> Result<(), CoreError> {
        let before = self.reasoning_part(&key).unwrap_or("").to_owned();
        if !text.starts_with(&before) {
            return err("stream_output_mismatch");
        }
        self.reasoning_pending.push_str(&text[before.len()..]);
        self.set_reasoning_part(key, text);
        Ok(())
    }
    /// Drains reasoning text accumulated since the last take; the stream
    /// layer emits it as one `run.message.reasoning-delta` per SSE frame,
    /// matching the TypeScript reference client.
    pub fn take_reasoning_delta(&mut self) -> Option<String> {
        if self.reasoning_pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.reasoning_pending))
        }
    }
    pub fn finish(mut self, tools: &[ToolDefinition]) -> Result<ResponsesResult, CoreError> {
        let terminal = self
            .terminal
            .take()
            .ok_or_else(|| CoreError::responses("stream_incomplete"))?;
        let output = terminal["output"]
            .as_array()
            .filter(|output| output.len() <= 1024)
            .ok_or_else(|| CoreError::responses("malformed_response"))?;
        let mut ids = HashSet::new();
        let mut call_ids = HashSet::new();
        let mut calls = vec![];
        let mut content = String::new();
        for (index, item) in output.iter().enumerate() {
            let id = nonempty(&item["id"])?;
            if !ids.insert(id.to_owned()) {
                return err("duplicate_output_item");
            }
            if let Some(state) = self.items.get(&index) {
                compare(state, item)?;
            }
            if self
                .items
                .values()
                .any(|state| state.id == id && state.index != index)
            {
                return err("output_item_mismatch");
            }
            if item
                .get("status")
                .is_some_and(|status| status != "completed")
            {
                return err("response_incomplete");
            }
            match string(&item["type"])? {
                "message" => content.push_str(&message_text(item)?),
                "function_call" => {
                    let call = tool_call(item, tools)?;
                    if !call_ids.insert(call.id.clone()) {
                        return err("duplicate_tool_call_id");
                    }
                    calls.push(call);
                }
                "reasoning" => {
                    if item["encrypted_content"].as_str().is_none_or(str::is_empty) {
                        return err("continuation_required");
                    }
                }
                _ => return err("unsupported_output"),
            }
        }
        if self.items.values().any(|state| !ids.contains(&state.id)) {
            return err("missing_output_item");
        }
        if !content.starts_with(&self.emitted) {
            return err("stream_output_mismatch");
        }
        // Frozen `ResponsesReasoning.finish` runs LAST (after every other
        // terminal check): each reasoning item's final summary/content
        // parts must EXTEND the streamed prefix for their key — a conflict
        // is `stream_output_mismatch` — and terminal-only reasoning (no
        // streamed delta for that part) is preserved, not dropped. The
        // public `reasoningContent` is the parts joined in first-appearance
        // order.
        for item in output {
            if item["type"] != "reasoning" {
                continue;
            }
            let id = nonempty(&item["id"])?.to_owned();
            for channel in ["summary", "content"] {
                let Some(parts) = item[channel].as_array() else {
                    continue;
                };
                for (index, value) in parts.iter().enumerate() {
                    let Some(text) = value.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    let key = format!("{id}:{channel}:{index}");
                    if !text.starts_with(self.reasoning_part(&key).unwrap_or("")) {
                        return err("stream_output_mismatch");
                    }
                    self.set_reasoning_part(key, text.to_owned());
                }
            }
        }
        let reasoning = self
            .reasoning_parts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>();
        Ok(ResponsesResult {
            output: output.clone(),
            content,
            calls,
            reasoning,
            usage: terminal
                .get("usage")
                .filter(|usage| !usage.is_null())
                .cloned(),
        })
    }
}
fn compare(state: &Item, final_item: &Value) -> Result<(), CoreError> {
    if state.id != final_item["id"] || state.item["type"] != final_item["type"] {
        return err("output_item_mismatch");
    }
    if final_item["type"] == "function_call" {
        if ["call_id", "name", "namespace"]
            .iter()
            .any(|key| state.item[*key] != final_item[*key])
        {
            return err("output_item_mismatch");
        }
        if !string(&final_item["arguments"])?.starts_with(&state.arguments)
            || state.done && state.item["arguments"] != final_item["arguments"]
        {
            return err("stream_output_mismatch");
        }
    }
    if final_item["type"] == "message" {
        for (index, text) in &state.texts {
            let part = &final_item["content"][*index];
            let value = if part["type"] == "refusal" {
                &part["refusal"]
            } else {
                &part["text"]
            };
            if !string(value)?.starts_with(text) {
                return err("stream_output_mismatch");
            }
        }
        if state.done && message_text(&state.item)? != message_text(final_item)? {
            return err("stream_output_mismatch");
        }
    }
    // Reasoning `encrypted_content` is deliberately not compared: OpenAI
    // re-encrypts it per event (fresh Fernet IV and timestamp), so
    // `output_item.done` and `response.completed` carry different ciphertexts
    // of the same reasoning. The terminal copy is authoritative.
    Ok(())
}
fn message_text(item: &Value) -> Result<String, CoreError> {
    if item["role"] != "assistant" {
        return err("malformed_response");
    }
    let parts = item["content"]
        .as_array()
        .ok_or_else(|| CoreError::responses("malformed_response"))?;
    let mut text = String::new();
    for part in parts {
        text.push_str(match part["type"].as_str() {
            Some("output_text") => string(&part["text"])?,
            Some("refusal") => string(&part["refusal"])?,
            _ => return err("unsupported_output"),
        });
    }
    Ok(text)
}
fn tool_call(item: &Value, tools: &[ToolDefinition]) -> Result<ToolCall, CoreError> {
    let name = string(&item["name"])?;
    let id = nonempty(&item["call_id"])?;
    let arguments = string(&item["arguments"])?;
    if item["namespace"] != "agentkit" || !tools.iter().any(|tool| tool.name == name) {
        return err("unknown_tool");
    }
    if agentkit_contracts::parse_javascript_json(arguments).is_err() {
        return err("malformed_tool_arguments");
    }
    Ok(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: arguments.into(),
    })
}
fn index(value: &Value) -> Result<usize, CoreError> {
    value
        .as_u64()
        .filter(|value| *value < 1024)
        .map(|value| value as usize)
        .ok_or_else(|| CoreError::responses("malformed_response"))
}
fn string(value: &Value) -> Result<&str, CoreError> {
    value
        .as_str()
        .ok_or_else(|| CoreError::responses("malformed_response"))
}
fn nonempty(value: &Value) -> Result<&str, CoreError> {
    let value = string(value)?;
    if value.is_empty() {
        return err("malformed_response");
    }
    Ok(value)
}
fn truthy(value: &Value) -> bool {
    !value.is_null() && value != &Value::Bool(false)
}
fn err<T>(code: &str) -> Result<T, CoreError> {
    Err(CoreError::responses(code))
}
