//! The managed-content HTTP lane: the routes the desktop backend previously
//! answered with "Native managed content services are not configured."
//! (external review §5). Each arm mirrors the frozen TypeScript route
//! handler in `packages/transport-http/src/routes/managed-content.ts` —
//! same request checks, same problem codes, same response shapes — and
//! delegates validation, ordering, CAS, and redaction to
//! `agentkit-store::managed_content`, the same engine the binding port
//! serves. `connectionAction` stays refused: it is the MCP-OAuth lifecycle
//! leg the binding documents as an open gap
//! (`docs/rust/native-binding-milestone.md` "Open gaps"), and a reduction
//! error is honest precisely because it is not a silent success.

use super::*;
use serde_json::{Value, json};

/// The documented MCP-OAuth reduction (verbatim in `docs/rust/parity.md`).
pub(super) const CONNECTION_ACTION_REDUCTION: &str = "MCP OAuth is not available in the native binding: connection lifecycle actions are refused (docs/rust/native-binding-milestone.md \"Open gaps\": no OAuth, deny-all secret resolution).";

impl DesktopBackend {
    pub(super) async fn managed_content(
        &self,
        input: &OperationRequest,
    ) -> Result<OperationResponse, Problem> {
        let store = self.host.store();
        match input.operation.as_str() {
            "listPromptTemplates" => Ok(output(
                200,
                encoded(
                    store
                        .list_prompt_templates()
                        .await
                        .map_err(managed_store_error)?,
                )?,
            )),
            "publishPromptTemplate" => {
                // Same pre-checks the TypeScript route makes before it hands
                // the body to the service; everything else is the engine's
                // (and its single refusal class maps to the route's single
                // 400: "Template is invalid or its revision already exists.").
                let body = input
                    .body
                    .as_object()
                    .ok_or_else(|| invalid_request("Invalid prompt template."))?;
                for field in ["id", "revision", "description"] {
                    if !body.get(field).is_some_and(Value::is_string) {
                        return Err(invalid_request("Invalid prompt template."));
                    }
                }
                if !body.get("argumentsSchema").is_some_and(Value::is_object)
                    || !body.get("messages").is_some_and(Value::is_array)
                {
                    return Err(invalid_request("Invalid prompt template."));
                }
                let template = store
                    .publish_prompt_template(input.body.clone())
                    .await
                    .map_err(|_| {
                        Problem::new(
                            400,
                            "invalid_template",
                            "Template is invalid or its revision already exists.",
                        )
                    })?;
                Ok(output(201, encoded(template)?))
            }
            "deletePromptTemplate" => {
                store
                    .delete_prompt_template(
                        input.params["templateId"].clone(),
                        input.params["revision"].clone(),
                    )
                    .await
                    .map_err(managed_store_error)?;
                Ok(output(204, Value::Null))
            }
            "renderPromptTemplate" => {
                if !input.body.is_object() {
                    return Err(invalid_request("Request body must be a JSON object."));
                }
                let selection = json!({
                    "id": input.params["templateId"],
                    "revision": input.params["revision"],
                    "arguments": input.body,
                });
                let rendered = store
                    .render_prompt_template_record(selection)
                    .await
                    .map_err(|_| {
                        Problem::new(
                            400,
                            "invalid_template_arguments",
                            "Template revision or arguments are invalid.",
                        )
                    })?;
                Ok(output(200, encoded(rendered)?))
            }
            "listConnections" => Ok(output(
                200,
                encoded(
                    store
                        .list_connections()
                        .await
                        .map_err(managed_store_error)?
                        .into_iter()
                        .map(connection_wire)
                        .collect::<Vec<_>>(),
                )?,
            )),
            "saveConnection" => {
                let body = input
                    .body
                    .as_object()
                    .ok_or_else(|| invalid_request("Request body must be a JSON object."))?;
                let config = body
                    .get("config")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid_request("config must be an object."))?;
                // Route-level request validation first, exactly like the
                // TypeScript handler: validator issues are the request's
                // fault (`invalid_request` with the validator's detail).
                agentkit_store::validate_connection_config(&Value::Object(config.clone()))
                    .map_err(managed_validation_error)?;
                let revision = positive_integer(&body["revision"]);
                let expected = match body.get("expectedRevision") {
                    None | Some(Value::Null) => Some(None),
                    Some(value) => positive_integer(value).map(Some),
                };
                if body.get("connectorId").is_none_or(|v| !v.is_string())
                    || body.get("accountId").is_none_or(|v| !v.is_string())
                    || !matches!(
                        body.get("workspaceId"),
                        None | Some(Value::Null) | Some(Value::String(_))
                    )
                    || revision.is_none()
                    || expected.is_none()
                {
                    return Err(invalid_request("Invalid connection identity or revision."));
                }
                let mut record = json!({
                    "id": input.params["connectionId"],
                    "connectorId": body["connectorId"],
                    "accountId": body["accountId"],
                    "revision": body["revision"],
                    "config": body["config"],
                });
                if let Some(workspace) = body.get("workspaceId").filter(|v| !v.is_null()) {
                    record["workspaceId"] = workspace.clone();
                }
                let saved = store
                    .save_connection(record, expected.flatten())
                    .await
                    .map_err(managed_store_error)?;
                Ok(output(200, encoded(connection_wire(saved))?))
            }
            "deleteConnection" => {
                store
                    .delete_connection(input.params["connectionId"].clone())
                    .await
                    .map_err(managed_store_error)?;
                Ok(output(204, Value::Null))
            }
            "listSkills" => Ok(output(
                200,
                encoded(store.list_skills().await.map_err(managed_store_error)?)?,
            )),
            "connectionAction" => Err(Problem::new(
                501,
                "not_implemented",
                CONNECTION_ACTION_REDUCTION,
            )),
            _ => Err(Problem::host(
                "internal_error",
                "Unknown managed content operation",
            )),
        }
    }
}

/// `ConnectionRecordSchema.workspaceId` is optional-but-not-nullable: drop a
/// null (e.g. from rows saved before the key was omitted) rather than emit it.
fn connection_wire(mut record: Value) -> Value {
    if let Some(object) = record.as_object_mut()
        && object.get("workspaceId").is_some_and(Value::is_null)
    {
        object.remove("workspaceId");
    }
    record
}

fn invalid_request(detail: impl Into<String>) -> Problem {
    Problem::new(400, "invalid_request", detail)
}

fn encoded(value: impl serde::Serialize) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(|e| Problem::host("internal_error", e.to_string()))
}

fn positive_integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.fract() == 0.0)
                .map(|n| n as i64)
        })
        .filter(|value| *value >= 1)
}
