use agentkit_client::{Call, Client, RestOperation, StreamOptions};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

mod support;

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn rust_client_talks_to_typescript_service_and_replays_same_key() {
    if support::ts_interop_skipped() {
        return;
    }
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts-service.ts");
    let child = Command::new("bun")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Bun is required for cross-language interoperability tests");
    let mut service = Service(child);
    let stdout = service.0.stdout.take().unwrap();
    let base = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        line
    })
    .await
    .unwrap();
    let client = Client::new(base.trim()).unwrap();
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
