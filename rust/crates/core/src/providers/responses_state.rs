use super::{responses_request::bounded, transport::classify};
use crate::{CoreError, ToolCall, ToolDefinition};
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
}
pub(crate) struct ResponsesResult {
    pub output: Vec<Value>,
    pub content: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Value>,
}
impl ResponsesState {
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
        Ok(())
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
        index(if kind.contains("summary") {
            &event["summary_index"]
        } else {
            &event["content_index"]
        })?;
        if kind.contains("_part.") {
            if event["part"]["type"] != "summary_text" {
                return err("malformed_response");
            }
            string(&event["part"]["text"])?;
        } else {
            string(if kind.ends_with(".delta") {
                &event["delta"]
            } else {
                &event["text"]
            })?;
        }
        Ok(())
    }
    pub fn finish(self, tools: &[ToolDefinition]) -> Result<ResponsesResult, CoreError> {
        let terminal = self
            .terminal
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
        Ok(ResponsesResult {
            output: output.clone(),
            content,
            calls,
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
    if state.done
        && final_item["type"] == "reasoning"
        && state.item["encrypted_content"] != final_item["encrypted_content"]
    {
        return err("stream_output_mismatch");
    }
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
