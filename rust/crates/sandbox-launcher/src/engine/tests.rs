//! The Engine API client against a scripted HTTP server on a unix socket.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use super::*;

/// Serve one canned response per connection, in order; every request
/// (head and body) is sent back on the channel.
fn server(socket: &std::path::Path, responses: Vec<String>) -> mpsc::UnboundedReceiver<String> {
    let listener = UnixListener::bind(socket).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            tx.send(request).unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = stream.shutdown().await;
        }
    });
    rx
}

async fn read_request(stream: &mut tokio::net::UnixStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        bytes.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&bytes).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_string)
                })
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length || n == 0 {
                return text;
            }
        }
        if n == 0 {
            return text;
        }
    }
}

fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn chunked(status: &str, body: &str) -> String {
    let (a, b) = body.split_at(body.len() / 2);
    format!(
        "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
        a.len(),
        b.len()
    )
}

fn api(dir: &tempfile::TempDir) -> (EngineApi, std::path::PathBuf) {
    let socket = dir.path().join("rt.sock");
    (EngineApi::new(socket.clone()), socket)
}

#[tokio::test]
async fn listing_sends_all_and_an_encoded_label_filter() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let body = r#"[{"Id":"abc123","State":"running","Labels":{"agentkit.sandbox.workspace":"ws-1"}},
                   {"Id":"def456","State":"exited","Labels":null}]"#;
    let mut requests = server(&socket, vec![chunked("200 OK", body)]);
    let listed = api
        .list_containers("agentkit.sandbox.workspace=ws-1")
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].is_running());
    assert!(listed[1].labels.is_empty());
    let request = requests.recv().await.unwrap();
    assert!(
        request.starts_with(
            "GET /v1.41/containers/json?all=true&filters=%7B%22label%22%3A%5B%22agentkit.sandbox.workspace%3Dws-1%22%5D%7D HTTP/1.1\r\n"
        ),
        "{request}"
    );
    assert!(
        request.to_ascii_lowercase().contains("host: localhost"),
        "{request}"
    );
}

#[tokio::test]
async fn create_posts_the_body_and_reads_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let mut requests = server(
        &socket,
        vec![
            reply("201 Created", r#"{"Id":"c0ffee","Warnings":null}"#),
            reply(
                "201 Created",
                r#"{"Id":"c0ffee2","Warnings":["limit discarded"]}"#,
            ),
        ],
    );
    let body = json!({ "Image": "x" });
    let created = api.create_container("aks-ws-1-g1", &body).await.unwrap();
    assert_eq!(created.id, "c0ffee");
    assert!(created.warnings.is_empty());
    let request = requests.recv().await.unwrap();
    assert!(request.starts_with("POST /v1.41/containers/create?name=aks-ws-1-g1 HTTP/1.1"));
    assert!(request.ends_with(r#"{"Image":"x"}"#), "{request}");
    let warned = api.create_container("aks-ws-1-g2", &body).await.unwrap();
    assert_eq!(warned.warnings, ["limit discarded"]);
}

#[tokio::test]
async fn runtime_errors_are_bounded_and_ids_checked() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let long = format!(r#"{{"message":"{}\u0007tail"}}"#, "x".repeat(500));
    let _requests = server(
        &socket,
        vec![
            reply("500 Internal Server Error", &long),
            reply("404 Not Found", "<html>no</html>"),
            reply("201 Created", r#"{"Id":"../../etc","Warnings":[]}"#),
        ],
    );
    let Err(RuntimeError::Status { status, message }) = api.start_container("abc").await else {
        panic!("expected a status error");
    };
    assert_eq!(status, 500);
    assert_eq!(message.chars().count(), crate::error::MESSAGE_MAX_CHARS);
    let Err(RuntimeError::Status { message, .. }) = api.start_container("abc").await else {
        panic!("expected a status error");
    };
    assert_eq!(message, "15 bytes of non-JSON body");
    let malformed = api.create_container("n", &json!({})).await.unwrap_err();
    assert!(
        matches!(malformed, RuntimeError::Malformed(_)),
        "{malformed}"
    );
    let refused = api.start_container("a/../b").await.unwrap_err();
    assert!(matches!(refused, RuntimeError::Malformed(_)));
}

#[tokio::test]
async fn statuses_map_to_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let _requests = server(
        &socket,
        vec![
            reply("304 Not Modified", ""),
            reply("404 Not Found", r#"{"message":"gone"}"#),
            reply("204 No Content", ""),
            reply("404 Not Found", r#"{"message":"gone"}"#),
            reply("409 Conflict", r#"{"message":"in use"}"#),
            reply("404 Not Found", r#"{"message":"gone"}"#),
            reply("200 OK", "{}"),
        ],
    );
    api.start_container("abc").await.unwrap();
    api.stop_container("abc", 2).await.unwrap();
    assert_eq!(api.remove_volume("v").await.unwrap(), Removal::Removed);
    assert_eq!(api.remove_volume("v").await.unwrap(), Removal::Absent);
    assert_eq!(api.remove_volume("v").await.unwrap(), Removal::InUse);
    assert!(!api.container_exists("aks-ws-1-g1").await.unwrap());
    assert!(api.container_exists("aks-ws-1-g1").await.unwrap());
}

#[tokio::test]
async fn volume_sizes_read_usage_data_from_either_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let podman = r#"{"Images":[{"Id":"x"}],"Containers":[],"Volumes":[
        {"Name":"aks-ws-a","UsageData":{"Size":1024000000,"RefCount":1}},
        {"Name":"aks-home-a","UsageData":{"Size":-1,"RefCount":0}},
        {"Name":"other"}]}"#;
    let mut requests = server(
        &socket,
        vec![
            reply("200 OK", podman),
            reply("200 OK", r#"{"Volumes":null}"#),
        ],
    );
    let sizes = api.volume_sizes().await.unwrap();
    assert_eq!(sizes["aks-ws-a"], Some(1_024_000_000));
    assert_eq!(sizes["aks-home-a"], None);
    assert_eq!(sizes["other"], None);
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /system/df?type=volume ")
    );
    assert!(api.volume_sizes().await.unwrap().is_empty());
}

#[tokio::test]
async fn volume_inspect_reads_driver_options_and_labels() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let docker = r#"{"Name":"v","Driver":"local","Labels":null,"Options":null,"Scope":"local"}"#;
    let bind = r#"{"Name":"v","Driver":"local","Labels":{"a":"b"},"Options":{"o":"bind"}}"#;
    let _requests = server(
        &socket,
        vec![
            reply("200 OK", docker),
            reply("200 OK", bind),
            reply("404 Not Found", r#"{"message":"no such volume"}"#),
        ],
    );
    let plain = api.inspect_volume("v").await.unwrap().unwrap();
    assert_eq!(plain.driver, "local");
    assert!(plain.options.is_empty() && plain.labels.is_empty());
    let bound = api.inspect_volume("v").await.unwrap().unwrap();
    assert_eq!(bound.options["o"], "bind");
    assert_eq!(api.inspect_volume("v").await.unwrap(), None);
}

#[tokio::test]
async fn a_silent_runtime_times_out_and_a_missing_one_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let transport = Transport::new(socket);
    let outcome = transport
        .call(Method::GET, "/_ping", None, Duration::from_millis(100))
        .await;
    assert_eq!(outcome.unwrap_err(), RuntimeError::Timeout);
    let missing = EngineApi::new(dir.path().join("absent.sock"));
    let error = missing.list_containers("a=b").await.unwrap_err();
    assert!(matches!(error, RuntimeError::Unreachable(_)), "{error}");
}

#[tokio::test]
async fn container_env_reads_the_inspected_config() {
    let dir = tempfile::tempdir().unwrap();
    let (api, socket) = api(&dir);
    let inspected =
        r#"{"Id":"abc","Config":{"Image":"x","Env":["A=1","AGENTKIT_CHANNEL_TOKEN=t"]}}"#;
    let mut requests = server(
        &socket,
        vec![
            reply("200 OK", inspected),
            reply("200 OK", r#"{"Id":"abc","Config":{"Env":null}}"#),
            reply("404 Not Found", r#"{"message":"no such container"}"#),
        ],
    );
    let env = api.container_env("abc").await.unwrap();
    assert_eq!(env, ["A=1", "AGENTKIT_CHANNEL_TOKEN=t"]);
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1.41/containers/abc/json HTTP/1.1")
    );
    assert!(api.container_env("abc").await.unwrap().is_empty());
    let gone = api.container_env("abc").await.unwrap_err();
    assert!(
        matches!(gone, RuntimeError::Status { status: 404, .. }),
        "{gone}"
    );
}
