//! AGENTKIT-56: the Rust MCP client's dispatch authority must match the
//! frozen TypeScript behavior — remote annotations are descriptive only
//! (effect defaults to `write`), arguments are validated against the tool
//! schema before dispatch, an exact-invocation authorization decision is
//! REQUIRED, and server-echoed content is scrubbed of resolved secrets.
#[path = "support/interop.rs"]
mod interop;
#[path = "support/managed.rs"]
mod support;
use agentkit_core::{CoreError, DispatchFence, DispatchGuard, ToolContext};
use agentkit_mcp::*;
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use support::*;

#[derive(Debug)]
struct Denied {
    checked: AtomicBool,
}
#[async_trait]
impl DispatchGuard for Denied {
    async fn check(&self) -> Result<(), CoreError> {
        self.checked.store(true, Ordering::SeqCst);
        Err(CoreError {
            code: "mcp_invocation_refused".into(),
            message: "the exact invocation was not authorized".into(),
            retryable: false,
        })
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 7,
        }
    }
}

fn unguarded() -> ToolContext {
    let mut context = support::context();
    context.dispatch_guard = None;
    context
}

/// (a) The fixture tool advertises `annotations.readOnlyHint: true`; that
/// alone must NOT raise the effect. Only the host's own `readOnly` config may.
#[tokio::test]
async fn remote_read_only_annotation_never_grants_read_effect() {
    let annotated = Arc::new(Connection::default());
    let (manager, _, _, _) = support::manager(vec![config("docs")], vec![Plan::Client(annotated)]);
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert_eq!(tool.definition().capability, "mcp.docs.remote");
    assert_eq!(tool.definition().effect, "write");
    manager.dispose().await.unwrap();

    let annotated = Arc::new(Connection::default());
    let mut declaration = config("docs");
    declaration.read_only = true;
    let (manager, _, _, _) = support::manager(vec![declaration], vec![Plan::Client(annotated)]);
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);
    assert_eq!(tool.definition().capability, "mcp.docs.remote");
    assert_eq!(tool.definition().effect, "read");
    manager.dispose().await.unwrap();
}

/// (b) Arguments are validated against the recorded tool schema locally; an
/// invalid invocation never reaches the wire.
#[tokio::test]
async fn invalid_arguments_are_refused_before_any_network_dispatch() {
    let live = Arc::new(Connection::default());
    live.lists.lock().unwrap().push_back(Ok(vec![
        serde_json::from_value(json!({
            "name": "remote",
            "inputSchema": {
                "type": "object",
                "properties": { "n": { "type": "number" }, "s": { "type": "string" } },
                "required": ["n"]
            }
        }))
        .unwrap(),
    ]));
    let (manager, _, connector, _) =
        manager(vec![config("docs")], vec![Plan::Client(live.clone())]);
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);

    let missing = tool.execute(context(), json!({"s": "x"})).await.unwrap();
    assert!(!missing.ok);
    assert_eq!(
        missing.model_data.unwrap()["errorCode"],
        "mcp_invalid_arguments"
    );
    let wrong_type = tool.execute(context(), json!({"n": "one"})).await.unwrap();
    assert!(!wrong_type.ok);
    assert_eq!(
        wrong_type.model_data.unwrap()["errorCode"],
        "mcp_invalid_arguments"
    );
    // The fixture server never saw a request for either refused invocation.
    assert_eq!(live.effects.load(Ordering::SeqCst), 0);
    assert!(live.names.lock().unwrap().is_empty());
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);

    let accepted = tool
        .execute(context(), json!({"n": 1, "s": "x"}))
        .await
        .unwrap();
    assert!(accepted.ok);
    assert_eq!(live.effects.load(Ordering::SeqCst), 1);
    assert_eq!(*live.names.lock().unwrap(), ["remote"]);
    manager.dispose().await.unwrap();
}

/// A tool whose schema the contracts compiler rejects cannot be validated,
/// so it must fail closed instead of dispatching unchecked.
#[tokio::test]
async fn uncompilable_schema_fails_closed_without_dispatch() {
    let live = Arc::new(Connection::default());
    live.lists.lock().unwrap().push_back(Ok(vec![
        serde_json::from_value(json!({
            "name": "remote",
            "inputSchema": { "$schema": "https://invalid.example/unsupported-dialect", "type": "object" }
        }))
        .unwrap(),
    ]));
    let (manager, _, connector, _) =
        manager(vec![config("docs")], vec![Plan::Client(live.clone())]);
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);
    let refused = tool.execute(context(), json!({})).await.unwrap();
    assert!(!refused.ok);
    assert_eq!(
        refused.model_data.unwrap()["errorCode"],
        "mcp_invalid_arguments"
    );
    assert_eq!(live.effects.load(Ordering::SeqCst), 0);
    assert!(live.names.lock().unwrap().is_empty());
    assert_eq!(connector.attempts.load(Ordering::SeqCst), 1);
    manager.dispose().await.unwrap();
}

/// (c) The pre-dispatch DispatchGuard check is REQUIRED on the execute path:
/// no guard, or a denying guard, produces the typed refusal and no dispatch.
#[tokio::test]
async fn exact_invocation_authorization_is_required_before_dispatch() {
    let live = Arc::new(Connection::default());
    let (manager, _, connector, _) =
        manager(vec![config("docs")], vec![Plan::Client(live.clone())]);
    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);

    let unattested = tool.execute(unguarded(), json!({})).await.unwrap();
    assert!(!unattested.ok);
    assert_eq!(
        unattested.model_data.unwrap()["errorCode"],
        "mcp_authority_revoked"
    );

    let denied = Arc::new(Denied {
        checked: AtomicBool::new(false),
    });
    let mut context = support::context();
    context.dispatch_guard = Some(denied.clone());
    let refused = tool.execute(context, json!({})).await.unwrap();
    assert!(!refused.ok);
    assert_eq!(
        refused.model_data.unwrap()["errorCode"],
        "mcp_authority_revoked"
    );
    assert!(
        denied.checked.load(Ordering::SeqCst),
        "guard must be consulted"
    );

    assert_eq!(live.effects.load(Ordering::SeqCst), 0);
    assert!(live.names.lock().unwrap().is_empty());
    assert_eq!(
        connector.attempts.load(Ordering::SeqCst),
        1,
        "refusals happen locally; no reconnect or dispatch"
    );
    manager.dispose().await.unwrap();
}

/// (d) A resolved secret the server echoes back — in tool-result text (and
/// structured content) and in a JSON-RPC error message — must surface
/// redacted through every host-visible field.
#[tokio::test]
async fn server_echoed_secrets_are_redacted_in_results_and_errors() {
    const SECRET: &str = "s3cr3t-echo";
    let live = Arc::new(Connection::default());
    let mut leaked = model::CallToolResult::success(vec![
        model::ContentBlock::text(format!("token {SECRET}")),
        model::ContentBlock::embedded_text("res://leak", format!("nested {SECRET}")),
    ]);
    leaked.structured_content = Some(json!({"leak": format!("structured {SECRET}")}));
    live.calls.lock().unwrap().extend([
        Ok(leaked),
        Err(McpClientError::Remote(format!(
            "upstream rejected {SECRET}"
        ))),
    ]);
    let mut declaration = config("docs");
    declaration
        .secret_refs
        .insert("token".into(), "wire".into());
    let (manager, secrets, _, _) = manager(vec![declaration], vec![Plan::Client(live.clone())]);
    secrets
        .values
        .lock()
        .unwrap()
        .insert("wire".into(), SECRET.into());

    let tool = manager.contribute(token()).await.unwrap().tools.remove(0);
    let result = tool.execute(context(), json!({})).await.unwrap();
    assert!(result.ok);
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(
        !serialized.contains(SECRET),
        "resolved secret leaked into the tool result: {serialized}"
    );
    assert!(serialized.contains("[redacted]"));
    // Resource text now reaches the model with untrusted source attribution;
    // it must receive the same secret redaction as ordinary text.
    assert_eq!(
        result.model_data.unwrap()["text"],
        "token [redacted]\n[Source: res://leak]\nnested [redacted]"
    );
    assert_eq!(result.sources[0]["metadata"]["trusted"], false);

    let failure = tool.execute(context(), json!({})).await.unwrap();
    assert!(!failure.ok);
    let model = failure.model_data.unwrap();
    assert_eq!(model["errorCode"], "mcp_remote_error");
    let message = model["errorMessage"].as_str().unwrap();
    assert!(
        !message.contains(SECRET),
        "secret leaked into the error: {message}"
    );
    assert!(message.contains("[redacted]"));
    // Redaction happens after real delivery: both invocations reached the
    // server (the error response itself is one dispatched call).
    assert_eq!(live.effects.load(Ordering::SeqCst), 2);
    manager.dispose().await.unwrap();
}

/// A real stdio wire test: the fixture echoes the resolved secret the client
/// substituted into its environment, both in result text and in a JSON-RPC
/// error response; the manager must scrub it from everything the host sees.
#[tokio::test]
async fn wire_echoed_secrets_are_redacted_through_the_manager() {
    if interop::ts_interop_skipped("mcp authority stdio fixture") {
        return;
    }
    const SECRET: &str = "s3cr3t-wire";
    #[derive(Default)]
    struct Wire;
    #[async_trait]
    impl McpConnector for Wire {
        async fn connect(
            &self,
            transport: TransportConfig,
            options: ClientOptions,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<Arc<dyn McpConnection>, McpClientError> {
            let TransportConfig::Stdio { command, args, env } = transport else {
                return Err(McpClientError::Configuration);
            };
            Ok(Arc::new(
                McpClient::stdio(&command, &args, &env, options).await?,
            ))
        }
    }
    let fixture = format!("{}/tests/fixtures/stdio.mjs", env!("CARGO_MANIFEST_DIR"));
    let mut declaration = config("wire");
    declaration.transport = TransportConfig::Stdio {
        command: "bun".into(),
        args: vec![fixture],
        env: BTreeMap::from([
            ("PATH".into(), std::env::var("PATH").unwrap()),
            ("SECRET".into(), "${secret}".into()),
        ]),
    };
    declaration
        .secret_refs
        .insert("secret".into(), "wire-secret".into());
    let secrets = Arc::new(Secrets::default());
    secrets
        .values
        .lock()
        .unwrap()
        .insert("wire-secret".into(), SECRET.into());
    let manager = Arc::new(
        McpClientManager::with_ports(
            vec![declaration],
            secrets,
            Arc::new(Wire),
            Arc::new(support::Clock::default()),
        )
        .unwrap(),
    );
    let tools = manager.contribute(token()).await.unwrap().tools;
    let echo = tools
        .iter()
        .find(|tool| tool.definition().capability == "mcp.wire.echo-secret")
        .expect("echo-secret contributed")
        .clone();
    let fail = tools
        .iter()
        .find(|tool| tool.definition().capability == "mcp.wire.fail-secret")
        .expect("fail-secret contributed")
        .clone();

    let result = echo.execute(context(), json!({})).await.unwrap();
    assert!(result.ok);
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(
        !serialized.contains(SECRET),
        "wire echo leaked the resolved secret: {serialized}"
    );
    assert!(serialized.contains("[redacted]"));

    let failure = fail.execute(context(), json!({})).await.unwrap();
    assert!(!failure.ok);
    let model = failure.model_data.unwrap();
    assert_eq!(model["errorCode"], "mcp_remote_error");
    let message = model["errorMessage"].as_str().unwrap();
    assert!(
        !message.contains(SECRET),
        "wire JSON-RPC error leaked the resolved secret: {message}"
    );
    assert!(message.contains("[redacted]"));
    manager.dispose().await.unwrap();
}
