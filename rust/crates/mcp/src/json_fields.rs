use crate::{json_boundary::map_object, model::*};
use rmcp::ErrorData;
use serde_json::Value;
use std::sync::Arc;

pub(crate) trait JsonFields {
    fn map_json(&mut self, map: fn(&Value) -> Value);
}
fn meta(value: &mut Option<MetaObject>, map: fn(&Value) -> Value) {
    if let Some(value) = value {
        map_object(&mut value.0, map);
    }
}
impl JsonFields for Tool {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        map_object(Arc::make_mut(&mut self.input_schema), map);
        if let Some(schema) = &mut self.output_schema {
            map_object(Arc::make_mut(schema), map);
        }
        meta(&mut self.meta, map);
    }
}
impl JsonFields for ContentBlock {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        match self {
            Self::Text(value) => meta(&mut value.meta, map),
            Self::Image(value) => meta(&mut value.meta, map),
            Self::Audio(value) => meta(&mut value.meta, map),
            Self::ResourceLink(value) => meta(&mut value.meta, map),
            Self::Resource(value) => {
                meta(&mut value.meta, map);
                value.resource.map_json(map);
            }
            _ => {}
        }
    }
}
impl JsonFields for ResourceContents {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        match self {
            Self::TextResourceContents { meta: value, .. }
            | Self::BlobResourceContents { meta: value, .. } => meta(value, map),
            _ => {}
        }
    }
}
impl JsonFields for CallToolResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        if let Some(value) = &mut self.structured_content {
            *value = map(value);
        }
        meta(&mut self.meta, map);
        for content in &mut self.content {
            content.map_json(map);
        }
    }
}
impl JsonFields for ListToolsResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        meta(&mut self.meta, map);
        for tool in &mut self.tools {
            tool.map_json(map);
        }
    }
}
impl JsonFields for ListResourcesResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        meta(&mut self.meta, map);
        for resource in &mut self.resources {
            meta(&mut resource.meta, map);
        }
    }
}
impl JsonFields for ReadResourceResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        meta(&mut self.meta, map);
        for content in &mut self.contents {
            content.map_json(map);
        }
    }
}
impl JsonFields for ListPromptsResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        meta(&mut self.meta, map);
        for prompt in &mut self.prompts {
            meta(&mut prompt.meta, map);
        }
    }
}
impl JsonFields for GetPromptResult {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        meta(&mut self.meta, map);
        for message in &mut self.messages {
            message.content.map_json(map);
        }
    }
}
impl JsonFields for ErrorData {
    fn map_json(&mut self, map: fn(&Value) -> Value) {
        if let Some(value) = &mut self.data {
            *value = map(value);
        }
    }
}
