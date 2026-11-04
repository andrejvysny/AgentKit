//! End-to-end CORS matrix through the real router, pinned against the
//! behavior of the retired TypeScript transport
//! (`packages/transport-http/src/handler.ts` + `cors.ts`).

use agentkit_transport_http::{
    Actor, Backend, Config, CorsConfig, CorsOrigins, OperationRequest, OperationResponse, Problem,
    router,
};
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default)]
struct Fake;
#[async_trait]
impl Backend for Fake {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Actor, Problem> {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|id| Actor {
                id: id.into(),
                metadata: BTreeMap::new(),
            })
            .ok_or_else(|| Problem::new(401, "unauthorized", "Missing identity"))
    }
    async fn authorize(&self, _: &OperationRequest) -> Result<(), Problem> {
        Ok(())
    }
    async fn dispatch(&self, input: OperationRequest) -> Result<OperationResponse, Problem> {
        Ok(OperationResponse::json(
            200,
            json!({"operation": input.operation}),
        ))
    }
    async fn events(
        &self,
        _: &Actor,
        _: &str,
        _: Option<f64>,
        _: usize,
    ) -> Result<Vec<Value>, Problem> {
        Ok(Vec::new())
    }
    async fn run_status(&self, _: &Actor, _: &str) -> Result<Option<String>, Problem> {
        Ok(None)
    }
}

async fn serve(cors: Option<CorsConfig>) -> String {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            socket,
            router(
                Arc::new(Fake),
                Config {
                    cors,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
    });
    format!("http://{address}")
}

fn cors(origins: CorsOrigins) -> CorsConfig {
    CorsConfig {
        origins,
        allow_headers: None,
        expose_headers: None,
        max_age_seconds: None,
    }
}

const ALLOWED: &str = "http://localhost:5173";
const FOREIGN: &str = "http://evil.example";

#[tokio::test]
async fn preflight_from_allowed_origin_answers_204_with_route_facts() {
    let base = serve(Some(CorsConfig {
        max_age_seconds: Some(600),
        ..cors(CorsOrigins::List(vec![ALLOWED.to_owned()]))
    }))
    .await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/chats"))
        .header("origin", ALLOWED)
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let headers = response.headers();
    assert_eq!(headers["access-control-allow-origin"], ALLOWED);
    assert_eq!(headers["vary"], "Origin");
    // Truth about THIS path, not the API in general: /v1/chats serves
    // GET+POST; OPTIONS itself must not leak into the allow list.
    let allow = headers["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(allow.contains("GET") && allow.contains("POST"));
    assert!(!allow.contains("OPTIONS"));
    assert!(!allow.contains("PUT"));
    assert_eq!(
        headers["access-control-allow-headers"],
        "Content-Type, Idempotency-Key, Last-Event-ID, Authorization"
    );
    assert_eq!(headers["access-control-max-age"], "600");
}

#[tokio::test]
async fn preflight_from_unallowed_origin_falls_through_to_405() {
    // Not 403: the request is served as if CORS were unconfigured, and
    // the OPTIONS verb itself is what no route claims.
    let base = serve(Some(cors(CorsOrigins::List(vec![ALLOWED.to_owned()])))).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/chats"))
        .header("origin", FOREIGN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    let headers = response.headers();
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert_eq!(headers["vary"], "Origin");
}

#[tokio::test]
async fn preflight_to_unknown_path_404s_and_is_still_decorated() {
    let base = serve(Some(cors(CorsOrigins::List(vec![ALLOWED.to_owned()])))).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/nope"))
        .header("origin", ALLOWED)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let headers = response.headers();
    assert_eq!(headers["access-control-allow-origin"], ALLOWED);
    assert_eq!(headers["vary"], "Origin");
    assert!(!headers.contains_key("access-control-allow-methods"));
}

#[tokio::test]
async fn ordinary_responses_carry_vary_always_and_allow_origin_only_when_matched() {
    let base = serve(Some(CorsConfig {
        expose_headers: Some(vec!["X-Total-Count".to_owned()]),
        ..cors(CorsOrigins::List(vec![ALLOWED.to_owned()]))
    }))
    .await;
    let client = reqwest::Client::new();
    let allowed = client
        .get(format!("{base}/v1/chats"))
        .header("authorization", "user-1")
        .header("origin", ALLOWED)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    let headers = allowed.headers();
    assert_eq!(headers["access-control-allow-origin"], ALLOWED);
    assert_eq!(headers["vary"], "Origin");
    assert_eq!(headers["access-control-expose-headers"], "X-Total-Count");

    let foreign = client
        .get(format!("{base}/v1/chats"))
        .header("authorization", "user-1")
        .header("origin", FOREIGN)
        .send()
        .await
        .unwrap();
    // Same 200 body — only the browser is allowed to care.
    assert_eq!(foreign.status(), 200);
    let headers = foreign.headers();
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert!(!headers.contains_key("access-control-expose-headers"));
    assert_eq!(headers["vary"], "Origin");

    // A backend refusal is decorated too — a CORS policy that skips the
    // 401 is a CORS error on the day something actually goes wrong.
    let refused = client
        .get(format!("{base}/v1/chats"))
        .header("origin", ALLOWED)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    let headers = refused.headers();
    assert_eq!(headers["access-control-allow-origin"], ALLOWED);
    assert_eq!(headers["vary"], "Origin");

    let originless = client
        .get(format!("{base}/v1/chats"))
        .header("authorization", "user-1")
        .send()
        .await
        .unwrap();
    let headers = originless.headers();
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert_eq!(headers["vary"], "Origin");
}

#[tokio::test]
async fn wildcard_echoes_literal_star_without_trusting_the_origin_value() {
    let base = serve(Some(cors(CorsOrigins::Any))).await;
    let response = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/chats"))
        .header("origin", FOREIGN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn unconfigured_server_never_touches_a_response() {
    let base = serve(None).await;
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/chats"))
        .header("authorization", "user-1")
        .header("origin", ALLOWED)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert!(!headers.contains_key("vary"));
    assert!(!headers.contains_key("access-control-allow-origin"));

    let preflight = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{base}/v1/chats"))
        .header("origin", ALLOWED)
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 405);
    assert!(
        !preflight
            .headers()
            .contains_key("access-control-allow-origin")
    );
}
