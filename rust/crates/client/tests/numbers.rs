use agentkit_client::{Call, Client, ClientError, RestOperation};
use agentkit_contracts::{javascript_number, parse_javascript_json};
use axum::{
    Router,
    body::Bytes,
    http::StatusCode,
    routing::{get, post},
};

const RAW: &str = r#"{"wide":9007199254740993,"overflow":1e400,"negativeZero":-0,"object":{"$serde_json::private::Number":"123"}}"#;

async fn start() -> (Client, tokio::task::JoinHandle<std::io::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let app = Router::new()
        .route("/v1/version", get(|| async { RAW }))
        .route("/v1/chats", post(|body: Bytes| async move { body }))
        .route(
            "/v1/settings",
            get(|| async { (StatusCode::BAD_REQUEST, RAW) }),
        );
    (
        client,
        tokio::spawn(axum::serve(listener, app).into_future()),
    )
}

#[tokio::test]
async fn client_uses_javascript_numbers_at_request_response_and_error_boundaries() {
    let (client, server) = start().await;
    let value = client
        .call(RestOperation::GetVersion, Call::default())
        .await
        .unwrap();
    assert_eq!(
        javascript_number(&value["wide"]),
        Some(9_007_199_254_740_992.0)
    );
    assert_eq!(javascript_number(&value["overflow"]), Some(f64::INFINITY));
    assert!(
        javascript_number(&value["negativeZero"])
            .unwrap()
            .is_sign_negative()
    );
    assert_eq!(value["object"]["$serde_json::private::Number"], "123");
    let typed: serde_json::Value = client
        .call_typed(RestOperation::GetVersion, Call::default())
        .await
        .unwrap();
    assert_eq!(typed["object"]["$serde_json::private::Number"], "123");
    let input = parse_javascript_json(&format!(r#"{{"metadata":{RAW}}}"#)).unwrap();
    let output = client
        .call(
            RestOperation::CreateChat,
            Call {
                body: Some(input),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(output["metadata"]["overflow"].is_null());
    assert_eq!(output["metadata"]["negativeZero"], 0);
    assert_eq!(
        output["metadata"]["object"]["$serde_json::private::Number"],
        "123"
    );
    let error = client
        .call(RestOperation::GetSettings, Call::default())
        .await
        .unwrap_err();
    let ClientError::Http { problem, .. } = error else {
        panic!("expected HTTP error");
    };
    assert_eq!(javascript_number(&problem["overflow"]), Some(f64::INFINITY));
    assert_eq!(problem["object"]["$serde_json::private::Number"], "123");
    server.abort();
}
