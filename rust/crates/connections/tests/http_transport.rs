#![cfg(feature = "http")]
mod common;
use agentkit_connections::*;
use common::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

fn server(response: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0; 8192];
        let size = stream.read(&mut request).unwrap();
        assert!(request[..size].starts_with(b"POST /v1/chat/completions HTTP/1.1"));
        stream.write_all(response.as_bytes()).unwrap();
    });
    (endpoint, thread)
}

fn http_native(endpoint: &str, limit: usize) -> (NativeConnections, Arc<CountingSecrets>) {
    let app = app(&ConnectionRepository::default(), "http-tests");
    let mut configuration = config();
    configuration.endpoint = endpoint.parse().unwrap();
    app.create(id(), configuration).unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let network = Arc::new(ReqwestConnectionNetwork::new(Duration::from_secs(2), limit).unwrap());
    (
        NativeConnections::new(app, secrets.clone(), network),
        secrets,
    )
}

fn http_input() -> PlanInput {
    let mut input = input();
    input.payload = br#"{"model":"model-one","messages":[]}"#.to_vec();
    input
}

#[test]
fn preflight_accepts_ignored_literal_private_number_objects_without_rewriting_payload() {
    let (native, secrets) = http_native("http://127.0.0.1:1/v1", 64);
    let mut input = http_input();
    input.payload = br#"{"model":"model-one","messages":[],"metadata":{"$serde_json::private::Number":"literal"}}"#.to_vec();
    let original = input.payload.clone();
    let plan = native.plan(&id(), input).unwrap();
    let network = ReqwestConnectionNetwork::new(Duration::from_secs(1), 64).unwrap();
    network.check_available(&plan).unwrap();
    assert_eq!(plan.payload(), original);
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approved_exact_endpoint_returns_bounded_response() {
    let (endpoint, thread) =
        server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    let (native, secrets) = http_native(&endpoint, 64);
    let destination = format!("{endpoint}/chat/completions").parse().unwrap();
    let plan = native.plan_to(&id(), http_input(), destination).unwrap();
    let consent = native.approve(&plan).unwrap();
    let response = native.execute(&plan, &consent).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"{}");
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 1);
    thread.join().unwrap();
}

#[tokio::test]
async fn extra_headers_resolve_from_secret_slot_and_reach_only_approved_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = [0; 8192];
        let size = stream.read(&mut bytes).unwrap();
        let request = String::from_utf8_lossy(&bytes[..size]).to_lowercase();
        assert!(request.contains("x-attribution: private-header-value\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let app = app(&ConnectionRepository::default(), "header-http");
    let mut configuration = config();
    configuration.endpoint = endpoint.parse().unwrap();
    configuration.credential_required = false;
    configuration
        .extra_header_names
        .insert("X-Attribution".into());
    app.create(id(), configuration).unwrap();
    let network = Arc::new(ReqwestConnectionNetwork::new(Duration::from_secs(2), 64).unwrap());
    let native = NativeConnections::new(app, Arc::new(MemorySecretStore::default()), network);
    native
        .provision_headers(
            &id(),
            std::collections::BTreeMap::from([(
                "X-Attribution".into(),
                Secret::new("private-header-value"),
            )]),
        )
        .await
        .unwrap();
    let destination = format!("{endpoint}/chat/completions").parse().unwrap();
    let plan = native.plan_to(&id(), http_input(), destination).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert_eq!(native.execute(&plan, &consent).await.unwrap().status, 200);
    thread.join().unwrap();
}

#[tokio::test]
async fn redirects_and_oversized_bodies_fail_closed() {
    for response in [
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
    ] {
        let (endpoint, thread) = server(response);
        let (native, _) = http_native(&endpoint, 4);
        let destination = format!("{endpoint}/chat/completions").parse().unwrap();
        let plan = native.plan_to(&id(), http_input(), destination).unwrap();
        let consent = native.approve(&plan).unwrap();
        assert!(matches!(
            native.execute(&plan, &consent).await,
            Err(ConnectionError::NetworkFailed)
        ));
        thread.join().unwrap();
    }
}

#[tokio::test]
async fn wrong_payload_model_and_unsupported_adapter_reject_before_secret_reads() {
    let (native, secrets) = http_native("http://127.0.0.1:1/v1", 64);
    let mut mismatched = http_input();
    mismatched.payload = br#"{"model":"other-model"}"#.to_vec();
    let plan = native.plan(&id(), mismatched).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::UnsupportedModel)
    ));
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
    let app = app(&ConnectionRepository::default(), "unavailable-http");
    let mut configuration = config();
    configuration.provider_kind = "unqualified-protocol".into();
    app.create(id(), configuration).unwrap();
    let network = Arc::new(ReqwestConnectionNetwork::new(Duration::from_secs(1), 64).unwrap());
    let native = NativeConnections::new(app, secrets.clone(), network);
    let plan = native.plan(&id(), http_input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::NetworkUnavailable)
    ));
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
}
