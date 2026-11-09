use crate::AgentTaskDto;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubmitTeammateTaskResponse {
    #[serde(rename = "task")]
    pub task: AgentTaskDto,
    #[serde(rename = "replayed")]
    pub replayed: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(SubmitTeammateTaskResponse, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        task: crate::required_field(&mut object, "task")?,
        replayed: crate::required_field(&mut object, "replayed")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
