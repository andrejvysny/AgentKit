use agentkit_client::{Call, Client, RestOperation, StreamOptions};
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;

mod support;

#[tokio::test]
async fn rust_client_talks_to_native_service_and_replays_same_key() {
    let server = support::Native::start(&support::Observer::default()).await;
    let client = Client::new(&server.base_url)
        .unwrap()
        .with_headers(support::bearer());
    let version = client
        .call(RestOperation::GetVersion, Call::default())
        .await
        .unwrap();
    assert_eq!(version["contractVersion"], "0.7.0");
    let chat = client
        .call(
            RestOperation::CreateChat,
            Call {
                body: Some(json!({"title":"Rust interop"})),
                ..Call::default()
            },
        )
        .await
        .unwrap();
    let chat_id = chat["id"].as_str().unwrap();
    let turn = client
        .submit_message(chat_id, json!({"content":"hello"}), "rust-interop-key")
        .await
        .unwrap();
    let replay = client
        .submit_message(chat_id, json!({"content":"hello"}), "rust-interop-key")
        .await
        .unwrap();
    assert_eq!(turn, replay);
    let run_id = turn["runId"].as_str().unwrap();
    let options = StreamOptions {
        retry_delay: std::time::Duration::from_millis(1),
        ..StreamOptions::default()
    };
    let stream = client.stream_run(run_id, options);
    let events: Vec<Value> = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        stream.map(|e| e.unwrap()).collect().await
    })
    .await
    .unwrap();
    assert!(events.iter().any(|e| e["type"] == "run.message.delta"));
    for (seq, event) in events.iter().enumerate() {
        assert_eq!(event["seq"].as_u64(), Some(seq as u64));
    }
    let run = client
        .call(
            RestOperation::GetRun,
            Call {
                path: BTreeMap::from([("runId".into(), run_id.into())]),
                ..Call::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run["status"], "completed");
}
