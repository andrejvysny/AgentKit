#[path = "support/interop.rs"]
mod interop;
use agentkit_mcp::{ClientOptions, ElicitationHandler, McpClient, model::*};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Host {
    requests: Mutex<Vec<Value>>,
    tokens: Mutex<Vec<CancellationToken>>,
    invalid: bool,
    pending: bool,
}
#[async_trait]
impl ElicitationHandler for Host {
    async fn elicit(
        &self,
        request: Value,
        cancellation: CancellationToken,
    ) -> Result<ElicitResult, ErrorData> {
        self.requests.lock().unwrap().push(request);
        self.tokens.lock().unwrap().push(cancellation);
        if self.pending {
            std::future::pending::<()>().await;
        }
        Ok(
            ElicitResult::new(ElicitationAction::Accept).with_content(if self.invalid {
                json!({"value":4})
            } else {
                json!({"value":"chosen"})
            }),
        )
    }
}

async fn open_client(host: Option<Arc<Host>>, timeout: Duration) -> McpClient {
    let environment = BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]);
    let options = ClientOptions {
        request_timeout: timeout,
        elicitation: host.map(|host| host as Arc<dyn ElicitationHandler>),
        ..Default::default()
    };
    McpClient::stdio(
        "bun",
        &[format!(
            "{}/tests/fixtures/elicitation-stdio.mjs",
            env!("CARGO_MANIFEST_DIR")
        )],
        &environment,
        options,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn optional_host_elicitation_is_advertised_and_returns_validated_user_content() {
    if interop::ts_interop_skipped("elicitation stdio fixture") {
        return;
    }
    let host = Arc::new(Host::default());
    let client = open_client(Some(host.clone()), Duration::from_secs(2)).await;
    let result = client
        .call_tool("ask", JsonObject::new(), CancellationToken::new())
        .await
        .unwrap();
    let content = serde_json::to_value(result.content).unwrap();
    let answer: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        answer,
        json!({"action":"accept","content":{"value":"chosen"}})
    );
    assert_eq!(
        host.requests.lock().unwrap()[0]["requestedSchema"]["required"],
        json!(["value"])
    );
    client.close().await.unwrap();
    let bare = open_client(None, Duration::from_secs(2)).await;
    let result = bare
        .call_tool(
            "ask",
            json!({"capabilities":true}).as_object().unwrap().clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let content = serde_json::to_value(result.content).unwrap();
    let capabilities: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert!(capabilities.get("elicitation").is_none());
    bare.close().await.unwrap();
}

#[tokio::test]
async fn invalid_user_content_fails_closed_and_timeout_cancels_host_scope() {
    if interop::ts_interop_skipped("elicitation stdio fixture") {
        return;
    }
    let host = Arc::new(Host {
        invalid: true,
        ..Default::default()
    });
    let client = open_client(Some(host), Duration::from_secs(2)).await;
    assert!(
        client
            .call_tool("ask", JsonObject::new(), CancellationToken::new())
            .await
            .is_err()
    );
    client.close().await.unwrap();
    let host = Arc::new(Host {
        pending: true,
        ..Default::default()
    });
    let client = open_client(Some(host.clone()), Duration::from_millis(100)).await;
    assert!(
        client
            .call_tool("ask", JsonObject::new(), CancellationToken::new())
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if host
                .tokens
                .lock()
                .unwrap()
                .first()
                .is_some_and(CancellationToken::is_cancelled)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    client.close().await.unwrap();
}
