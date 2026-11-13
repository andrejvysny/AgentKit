use agentkit_client::{Call, Client, RestOperation as Op, StreamOptions};
use agentkit_contracts::REST_ROUTES;
#[path = "all_routes/mod.rs"]
mod scenarios;
mod support;
use futures::StreamExt;
use scenarios::{
    cancellation_and_resume, chats, management, optional_services, proposals, providers,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
    time::Duration,
};

struct Fixture {
    server: support::Native,
    observer: support::Observer,
    client: Client,
    covered: BTreeMap<String, u16>,
    expected_requests: BTreeMap<String, (String, u16)>,
}
impl Fixture {
    async fn start() -> Self {
        let observer = support::Observer::default();
        let server = support::Native::start(&observer).await;
        let mut headers = support::bearer();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-agentkit-operation"),
            "streamRun".parse().unwrap(),
        );
        let client = Client::new(&server.base_url).unwrap().with_headers(headers);
        Self {
            server,
            observer,
            client,
            covered: BTreeMap::new(),
            expected_requests: BTreeMap::new(),
        }
    }
    async fn call(&mut self, op: Op, mut call: Call, schema: &str, status: u16) -> Value {
        self.covered.insert(op.as_str().into(), status);
        let case = self.expected_requests.len().to_string();
        self.expected_requests
            .insert(case.clone(), (op.as_str().into(), status));
        call.headers
            .insert("x-agentkit-case", case.parse().unwrap());
        call.headers
            .insert("x-agentkit-operation", op.as_str().parse().unwrap());
        let value = self
            .client
            .call(op, call)
            .await
            .unwrap_or_else(|error| panic!("{op}: {error}"));
        check(&value, schema);
        value
    }
    /// A native route that refuses by design: the client must surface the
    /// problem status, and the server must have answered with it.
    async fn refused(&mut self, op: Op, mut call: Call, status: u16) {
        self.covered.insert(op.as_str().into(), status);
        let case = self.expected_requests.len().to_string();
        self.expected_requests
            .insert(case.clone(), (op.as_str().into(), status));
        call.headers
            .insert("x-agentkit-case", case.parse().unwrap());
        call.headers
            .insert("x-agentkit-operation", op.as_str().parse().unwrap());
        let result = self.client.call(op, call).await;
        assert!(
            matches!(
                result,
                Err(agentkit_client::ClientError::Http { status: got, .. }) if got == status
            ),
            "{op}: {result:?}"
        );
    }
    /// A request the typed client cannot make (the agent-task SSE body, and
    /// the three routes whose contract declares no request schema yet take a
    /// body), tagged like every other call so the server's record can be
    /// audited.
    async fn raw(&mut self, op: Op, path: &str, body: Option<Value>, status: u16) -> String {
        self.covered.insert(op.as_str().into(), status);
        let case = self.expected_requests.len().to_string();
        self.expected_requests
            .insert(case.clone(), (op.as_str().into(), status));
        let route = op.route();
        let method = reqwest::Method::from_bytes(route.method.as_str().as_bytes()).unwrap();
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.server.base_url))
            .bearer_auth(support::TOKEN)
            .header("x-agentkit-case", case)
            .header("x-agentkit-operation", op.as_str());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        response.text().await.unwrap()
    }
    async fn drain(&mut self, run: &str) -> Vec<Value> {
        self.covered.insert(Op::StreamRun.as_str().into(), 200);
        tokio::time::timeout(
            Duration::from_secs(10),
            self.client
                .stream_run(run, StreamOptions::default())
                .map(|event| event.unwrap())
                .collect(),
        )
        .await
        .unwrap()
    }
    async fn verify(&self) {
        let expected: BTreeSet<_> = REST_ROUTES
            .iter()
            .map(|route| route.operation.as_str().to_owned())
            .collect();
        assert_eq!(
            self.covered.keys().cloned().collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(self.covered.len(), 63);
        let requests = self.observer.evidence.lock().unwrap().clone();
        let mut observed = BTreeSet::new();
        let mut observed_cases = BTreeSet::new();
        for request in &requests {
            let op = request["operation"].as_str().unwrap();
            let expected_status = if op == Op::StreamRun.as_str() {
                200
            } else {
                let case = request["case"].as_str().unwrap();
                assert!(observed_cases.insert(case.to_owned()));
                let (expected_op, status) = &self.expected_requests[case];
                assert_eq!(op, expected_op);
                *status
            };
            assert_eq!(
                request["status"].as_u64(),
                Some(u64::from(expected_status)),
                "{request}"
            );
            let route = op.parse::<Op>().unwrap().route();
            assert_eq!(request["method"], route.method.as_str());
            observed.insert(op.to_owned());
        }
        assert_eq!(observed, expected);
        assert_eq!(
            observed_cases,
            self.expected_requests.keys().cloned().collect()
        );
        assert_eq!(
            self.observer.applied.load(Ordering::SeqCst),
            1,
            "same operation ID must not apply twice"
        );
    }
}
fn request(path: &[(&str, &str)], body: Option<Value>) -> Call {
    Call {
        path: path
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        body,
        ..Call::default()
    }
}
fn agent_definition() -> Value {
    serde_json::json!({
        "name": "all-routes-agent",
        "purpose": "exercise the agent routes",
        "instructions": "none",
        "resources": [],
        "skills": [],
        "permissionMode": "ask",
        "allowedTools": [],
        "budgets": {},
        "secretRefs": [],
    })
}
fn check(value: &Value, schema: &str) {
    if schema == "*" {
        // The contract bundle has no response schema for this route.
    } else if schema.is_empty() {
        assert!(value.is_null());
    } else if let Some(item) = schema.strip_suffix("[]") {
        for value in value.as_array().unwrap() {
            agentkit_contracts::validate_schema(item, value).unwrap();
        }
    } else {
        agentkit_contracts::validate_schema(schema, value)
            .unwrap_or_else(|error| panic!("{schema}: {error:?} {value}"));
    }
}
fn id(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap().into()
}

#[tokio::test]
async fn rust_client_exercises_all_63_operations_against_the_native_host() {
    let mut fixture = Fixture::start().await;
    let chat = chats(&mut fixture).await;
    providers(&mut fixture).await;
    management(&mut fixture, &chat).await;
    proposals(&mut fixture).await;
    cancellation_and_resume(&mut fixture).await;
    fixture
        .call(Op::DeleteChat, request(&[("chatId", &chat)], None), "", 204)
        .await;
    optional_services(&mut fixture).await;
    fixture.verify().await;
}
