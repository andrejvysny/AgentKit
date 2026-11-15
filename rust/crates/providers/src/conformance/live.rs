//! Live endpoint model. Refuses to exist unless the caller passes the explicit
//! live flag. The key is a [`Secret`]: never printed, scrubbed from every error.
use super::runner::{Model, ModelCall, ModelTurn, RunCtx};
use super::task::Task;
use crate::probe::{Endpoint, Secret};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Endpoint settings for a live run. `Debug` never shows the key.
#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub base_url: String,
    pub model: String,
    pub key: Secret,
}

pub struct LiveModel {
    endpoint: Endpoint,
}

/// Builds the live model only when `live_flag` is set (the CLI's `--live`).
pub fn live_model(config: LiveConfig, live_flag: bool) -> Result<LiveModel, String> {
    if !live_flag {
        return Err(
            "refusing to contact a live endpoint without the explicit --live flag".to_owned(),
        );
    }
    Ok(LiveModel {
        endpoint: Endpoint::new(&config.base_url, &config.model, config.key),
    })
}

fn tool_defs(task: &Task) -> Vec<Value> {
    task.allowed_tools
        .iter()
        .map(|tool| {
            let properties: serde_json::Map<String, Value> = tool
                .required
                .iter()
                .chain(&tool.optional)
                .map(|name| (name.clone(), json!({"type":"string"})))
                .collect();
            json!({"type":"function","function":{
                "name": tool.name,
                "description": format!("Conformance tool {}", tool.name),
                "parameters": {"type":"object","properties":properties,"required":tool.required,"additionalProperties":false}
            }})
        })
        .collect()
}

#[async_trait]
impl Model for LiveModel {
    fn name(&self) -> &str {
        &self.endpoint.model
    }

    async fn turn(
        &self,
        task: &Task,
        _ctx: &RunCtx,
        messages: &[Value],
    ) -> Result<ModelTurn, String> {
        let body = self.endpoint.chat_body(
            Value::Array(messages.to_vec()),
            json!({"tools": tool_defs(task), "tool_choice": "auto", "temperature": 0}),
        );
        let reply = self.endpoint.chat(&body).await?;
        if !reply.ok() {
            return Err(format!("HTTP {}: {}", reply.status, reply.text));
        }
        let message = reply.message();
        let calls = message["tool_calls"]
            .as_array()
            .map(|list| {
                list.iter()
                    .enumerate()
                    .map(|(i, call)| ModelCall {
                        id: call["id"]
                            .as_str()
                            .map_or_else(|| format!("call_{i}"), str::to_owned),
                        name: call["function"]["name"].as_str().unwrap_or_default().to_owned(),
                        arguments: call["function"]["arguments"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ModelTurn {
            text: message["content"].as_str().unwrap_or_default().to_owned(),
            calls,
        })
    }
}
