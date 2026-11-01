//! The managed-content port surface: `promptTemplates.*`, `skills.*`,
//! `connections.*`, and `runContent.*`.
//!
//! These are the arms the binding previously answered with "Native managed
//! content services are not configured." (external review §5). Like every
//! other namespace here, the arm parses → calls the engine → serializes:
//! validation, ordering, CAS, hashing, and tombstone semantics all live in
//! `agentkit-store::managed_content` (the TypeScript services' contract),
//! and nothing here runs an effect the input did not authorize. Every arm
//! is transaction-compatible through the same `ports::execute` the
//! transaction handle uses.
//!
//! Connection lifecycle (`connect`/`reconnect`/`test`) is deliberately
//! absent: that leg is the documented MCP-OAuth reduction
//! (`docs/rust/native-binding-milestone.md` "Open gaps") and the HTTP lane
//! refuses it as `not_implemented` — this surface never pretends to connect.

use super::ports::{record, records, text};
use crate::errors;
use agentkit_store::{SECRET_REF_MARKER, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

/// Engine refusals carry one stable code each. `NotFound` maps to
/// `not_found`; a compare-and-swap loss is a state violation
/// (`invalid_state`); every validation refusal — including the reference's
/// single 400 class for "template invalid or revision exists" — maps to
/// `invalid_input`. The deny-all `secretRefs` refusal keeps the binding's
/// dedicated secret code.
fn port_error(error: agentkit_store::StoreError) -> Error {
    if matches!(&error, agentkit_store::StoreError::InvalidInput(message)
        if message.starts_with(SECRET_REF_MARKER))
    {
        return errors::stable("mcp_secret_missing", error.to_string(), None);
    }
    errors::store_port(error, "invalid_state", "invalid_input")
}

/// Strict wire shape: an unknown option is a refusal, never an ignore.
fn only(input: &Value, allowed: &[&str]) -> Result<()> {
    let Some(object) = input.as_object() else {
        return Err(errors::invalid("input must be an object"));
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(errors::invalid(format!("unknown field {key}")));
    }
    Ok(())
}

fn optional_text<'a>(input: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(errors::invalid(format!("{field} must be a string"))),
    }
}

fn object_field<'a>(input: &'a Value, field: &str) -> Result<&'a Value> {
    if input[field].is_object() {
        Ok(&input[field])
    } else {
        Err(errors::invalid(format!("{field} must be an object")))
    }
}

fn string_array(input: &Value, field: &str) -> Result<Vec<String>> {
    input[field]
        .as_array()
        .ok_or_else(|| errors::invalid(format!("{field} must be an array")))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| errors::invalid(format!("{field} must hold strings")))
        })
        .collect()
}

pub(super) async fn execute(
    store: &SqliteStore,
    namespace: &str,
    method: &str,
    input: Value,
) -> Result<Value> {
    match (namespace, method) {
        // -- prompt templates ------------------------------------------------
        ("promptTemplates", "publish") => {
            only(&input, &["template"])?;
            let template = object_field(&input, "template")?.clone();
            Ok(record(
                store
                    .publish_prompt_template(template)
                    .await
                    .map_err(port_error)?,
            ))
        }
        ("promptTemplates", "list") => {
            only(&input, &[])?;
            Ok(records(
                store.list_prompt_templates().await.map_err(port_error)?,
            ))
        }
        ("promptTemplates", "get") => {
            only(&input, &["id", "revision"])?;
            Ok(record(
                store
                    .get_prompt_template(
                        text(&input, "id")?.to_owned(),
                        text(&input, "revision")?.to_owned(),
                    )
                    .await
                    .map_err(port_error)?
                    .unwrap_or(Value::Null),
            ))
        }
        ("promptTemplates", "remove") => {
            only(&input, &["id", "revision"])?;
            store
                .delete_prompt_template(
                    text(&input, "id")?.to_owned(),
                    text(&input, "revision")?.to_owned(),
                )
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        ("promptTemplates", "render") => {
            only(&input, &["id", "revision", "arguments"])?;
            let selection = json!({
                "id": input["id"],
                "revision": input["revision"],
                "arguments": object_field(&input, "arguments")?.clone(),
            });
            Ok(record(
                store
                    .render_prompt_template_record(selection)
                    .await
                    .map_err(port_error)?,
            ))
        }
        // -- managed skills ---------------------------------------------------
        ("skills", "publish") => {
            only(&input, &["source", "path", "content"])?;
            Ok(record(
                store
                    .publish_skill(
                        text(&input, "source")?.to_owned(),
                        text(&input, "path")?.to_owned(),
                        text(&input, "content")?.to_owned(),
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        ("skills", "list") => {
            only(&input, &[])?;
            Ok(records(store.list_skills().await.map_err(port_error)?))
        }
        ("skills", "get") => {
            only(&input, &["id"])?;
            Ok(record(
                store
                    .get_skill(text(&input, "id")?.to_owned())
                    .await
                    .map_err(port_error)?
                    .unwrap_or(Value::Null),
            ))
        }
        ("skills", "remove") => {
            only(&input, &["id"])?;
            store
                .delete_skill(text(&input, "id")?.to_owned())
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        // -- managed connections (non-secret configuration only) --------------
        ("connections", "list") => {
            only(&input, &[])?;
            Ok(records(store.list_connections().await.map_err(port_error)?))
        }
        ("connections", "get") => {
            only(&input, &["connectionId"])?;
            Ok(record(
                store
                    .get_connection(text(&input, "connectionId")?.to_owned())
                    .await
                    .map_err(port_error)?
                    .unwrap_or(Value::Null),
            ))
        }
        ("connections", "save") => {
            only(&input, &["record", "expectedRevision"])?;
            let candidate = object_field(&input, "record")?.clone();
            let expected = match input.get("expectedRevision") {
                None | Some(Value::Null) => None,
                Some(Value::Number(value)) => Some(value.as_i64().ok_or_else(|| {
                    errors::invalid("expectedRevision must be an integer or null")
                })?),
                Some(_) => {
                    return Err(errors::invalid(
                        "expectedRevision must be an integer or null",
                    ));
                }
            };
            Ok(record(
                store
                    .save_connection(candidate, expected)
                    .await
                    .map_err(port_error)?,
            ))
        }
        ("connections", "remove") => {
            only(&input, &["connectionId"])?;
            store
                .delete_connection(text(&input, "connectionId")?.to_owned())
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        // -- run content --------------------------------------------------------
        ("runContent", "resolve") => {
            only(
                &input,
                &["selection", "authorizedConnectors", "tolerateUnavailable"],
            )?;
            let selection = object_field(&input, "selection")?.clone();
            let authorized = string_array(&input, "authorizedConnectors")?;
            let tolerate = match input.get("tolerateUnavailable") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                Some(_) => {
                    return Err(errors::invalid("tolerateUnavailable must be a boolean"));
                }
            };
            Ok(record(
                store
                    .resolve_run_content(selection, authorized, tolerate)
                    .await
                    .map_err(port_error)?,
            ))
        }
        ("runContent", "recordResource") => {
            only(&input, &["skill", "path", "revision"])?;
            let skill = object_field(&input, "skill")?.clone();
            store
                .record_run_content_resource(
                    skill,
                    text(&input, "path")?.to_owned(),
                    text(&input, "revision")?.to_owned(),
                )
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        ("runContent", "listResources") => {
            only(&input, &["skillId"])?;
            Ok(records(
                store
                    .list_run_content_resources(
                        optional_text(&input, "skillId")?.map(str::to_owned),
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported managed content store operation",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{begin, call, code, store};
    use serde_json::{Value, json};

    const HEX64: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn template(id: &str, revision: &str, content: &str) -> Value {
        json!({
            "id": id,
            "revision": revision,
            "description": "greeting",
            "argumentsSchema": { "type": "object", "properties": { "name": { "type": "string" } } },
            "messages": [{ "role": "user", "content": content }],
        })
    }

    fn skill_text(name: &str, requires: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: does things\nmetadata:\n  requires-connectors: {requires}\n---\nbody of {name}\n"
        )
    }

    fn connection(id: &str, revision: i64, alias: &str) -> Value {
        json!({
            "id": id,
            "connectorId": "github",
            "accountId": "acc",
            "revision": revision,
            "config": { "alias": alias, "transport": { "kind": "stdio", "command": "run", "env": { "TOKEN": "literal" } } },
        })
    }

    #[tokio::test]
    async fn template_lifecycle_through_the_port_surface() {
        let store = store();
        let published = call(
            &store,
            "promptTemplates.publish",
            json!({ "template": template("greet", "1", "Hi {{name}}") }),
        )
        .await
        .expect("publish");
        assert_eq!(published["id"], json!("greet"));
        call(
            &store,
            "promptTemplates.publish",
            json!({ "template": template("greet", "2", "Hello {{name}}") }),
        )
        .await
        .expect("second revision");
        let listed = call(&store, "promptTemplates.list", json!({}))
            .await
            .expect("list");
        assert_eq!(
            listed
                .as_array()
                .expect("array")
                .iter()
                .map(|t| format!(
                    "{}@{}",
                    t["id"].as_str().expect("id"),
                    t["revision"].as_str().expect("revision")
                ))
                .collect::<Vec<_>>(),
            vec!["greet@1", "greet@2"]
        );
        let error = call(
            &store,
            "promptTemplates.publish",
            json!({ "template": template("greet", "1", "again") }),
        )
        .await
        .expect_err("immutable revision");
        assert_eq!(code(error), "invalid_input");
        let rendered = call(
            &store,
            "promptTemplates.render",
            json!({ "id": "greet", "revision": "1", "arguments": { "name": "ada" } }),
        )
        .await
        .expect("render");
        assert_eq!(rendered["messages"][0]["content"], json!("Hi ada"));
        assert_eq!(rendered["provenance"]["revision"], json!("1"));
        let error = call(
            &store,
            "promptTemplates.render",
            json!({ "id": "greet", "revision": "9", "arguments": {} }),
        )
        .await
        .expect_err("unknown revision");
        assert_eq!(code(error), "not_found");
        call(
            &store,
            "promptTemplates.remove",
            json!({ "id": "greet", "revision": "1" }),
        )
        .await
        .expect("remove");
        assert_eq!(
            call(
                &store,
                "promptTemplates.get",
                json!({ "id": "greet", "revision": "1" })
            )
            .await
            .expect("get"),
            Value::Null
        );
        let error = call(
            &store,
            "promptTemplates.publish",
            json!({ "template": template("greet", "1", "x") }),
        )
        .await
        .expect_err("tombstone reserves the revision");
        assert_eq!(code(error), "invalid_input");
        // Unknown fields are refused, never ignored.
        let error = call(&store, "promptTemplates.list", json!({ "limit": 5 }))
            .await
            .expect_err("strict input");
        assert_eq!(code(error), "invalid_input");
    }

    #[tokio::test]
    async fn skill_catalog_and_run_content_resolve() {
        let store = store();
        let entry = call(
            &store,
            "skills.publish",
            json!({ "source": "docs", "path": "review", "content": skill_text("reviewer", "github") }),
        )
        .await
        .expect("publish");
        assert_eq!(entry["id"], json!("docs:review"));
        assert!(
            entry["connectorRequirements"]
                .as_array()
                .unwrap()
                .contains(&json!("github"))
        );
        let resolved = call(
            &store,
            "runContent.resolve",
            json!({
                "selection": { "skills": [{ "id": "docs:review", "revision": entry["revision"] }] },
                "authorizedConnectors": ["github"],
            }),
        )
        .await
        .expect("resolve");
        assert_eq!(resolved["blocks"][0]["title"], json!("Skill: reviewer"));
        assert_eq!(resolved["blocks"][0]["priority"], json!(100));
        let error = call(
            &store,
            "runContent.resolve",
            json!({
                "selection": { "skills": [{ "id": "docs:review", "revision": entry["revision"] }] },
                "authorizedConnectors": [],
            }),
        )
        .await
        .expect_err("connector gate");
        assert_eq!(code(error), "invalid_input");
        let tolerated = call(
            &store,
            "runContent.resolve",
            json!({
                "selection": { "skills": [{ "id": "docs:review", "revision": "wrong" }] },
                "authorizedConnectors": ["github"],
                "tolerateUnavailable": true,
            }),
        )
        .await
        .expect("tolerated");
        assert_eq!(tolerated["unavailable"][0]["id"], json!("docs:review"));
        let error = call(
            &store,
            "runContent.resolve",
            json!({
                "selection": { "skills": [
                    { "id": "docs:review", "revision": entry["revision"] },
                    { "id": "docs:review", "revision": entry["revision"] },
                ] },
                "authorizedConnectors": ["github"],
            }),
        )
        .await
        .expect_err("duplicate skill identity");
        assert_eq!(code(error), "invalid_input");
        call(
            &store,
            "runContent.recordResource",
            json!({ "skill": { "id": "docs:review", "revision": entry["revision"] }, "path": "references/guide.md", "revision": HEX64 }),
        )
        .await
        .expect("record");
        let resources = call(
            &store,
            "runContent.listResources",
            json!({ "skillId": "docs:review" }),
        )
        .await
        .expect("resources");
        assert_eq!(resources.as_array().expect("array").len(), 1);
        let error = call(
            &store,
            "runContent.recordResource",
            json!({ "skill": { "id": "docs:review", "revision": entry["revision"] }, "path": "../escape", "revision": HEX64 }),
        )
        .await
        .expect_err("path containment");
        assert_eq!(code(error), "invalid_input");
        call(&store, "skills.remove", json!({ "id": "docs:review" }))
            .await
            .expect("remove");
        assert!(
            call(&store, "skills.list", json!({}))
                .await
                .expect("list")
                .as_array()
                .expect("array")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn connection_cas_deny_all_and_redaction_through_the_port() {
        let store = store();
        let saved = call(
            &store,
            "connections.save",
            json!({ "record": connection("c1", 1, " github "), "expectedRevision": null }),
        )
        .await
        .expect("create");
        assert_eq!(saved["status"], json!("disconnected"));
        assert_eq!(saved["config"]["alias"], json!("github"));
        assert_eq!(saved["config"]["transport"]["env"]["TOKEN"], json!("***"));
        let error = call(
            &store,
            "connections.save",
            json!({ "record": connection("c1", 1, "github"), "expectedRevision": null }),
        )
        .await
        .expect_err("create-only CAS");
        assert_eq!(code(error), "invalid_state");
        let error = call(
            &store,
            "connections.save",
            json!({
                "record": {
                    "id": "c2", "connectorId": "github", "accountId": "acc", "revision": 1,
                    "config": { "alias": "other", "transport": { "kind": "stdio", "command": "x" }, "secretRefs": { "token": "ref" } },
                },
                "expectedRevision": null,
            }),
        )
        .await
        .expect_err("deny-all secretRefs");
        assert_eq!(code(error), "mcp_secret_missing");
        let listed = call(&store, "connections.list", json!({}))
            .await
            .expect("list");
        assert_eq!(listed.as_array().expect("array").len(), 1);
        call(
            &store,
            "connections.remove",
            json!({ "connectionId": "c1" }),
        )
        .await
        .expect("remove");
        assert_eq!(
            call(&store, "connections.get", json!({ "connectionId": "c1" }))
                .await
                .expect("get"),
            Value::Null
        );
    }

    #[tokio::test]
    async fn committed_writes_become_visible_and_rollbacks_leave_no_trace() {
        let store = store();
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "promptTemplates.publish",
            json!({ "template": template("tx", "1", "x") }),
        )
        .await
        .expect("publish in transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "promptTemplates.list", json!({}))
                .await
                .expect("list")
                .as_array()
                .expect("array")
                .is_empty()
        );
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "promptTemplates.publish",
            json!({ "template": template("tx", "1", "x") }),
        )
        .await
        .expect("publish again");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "promptTemplates.list", json!({}))
                .await
                .expect("list")
                .as_array()
                .expect("array")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_namespace_and_method_are_refused() {
        let store = store();
        let error = call(&store, "promptTemplates.destroy", json!({}))
            .await
            .expect_err("unknown method");
        assert_eq!(code(error), "not_implemented");
        let error = call(&store, "managedContent.list", json!({}))
            .await
            .expect_err("unknown namespace");
        assert_eq!(code(error), "not_implemented");
    }
}
