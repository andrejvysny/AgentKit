use agentkit_client::{Call, Client, RestOperation as Op, StreamOptions};
use agentkit_contracts::REST_ROUTES;
#[path = "all_routes/mod.rs"]
mod scenarios;
use futures::StreamExt;
use scenarios::{cancellation_and_resume, chats, management, proposals, providers};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Announcement {
    base_url: String,
    token: String,
    evidence_url: String,
    evidence_token: String,
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0
            .wait()
            .expect("TypeScript fixture process must be reaped");
    }
}
struct Fixture {
    _child: ChildGuard,
    client: Client,
    announcement: Announcement,
    covered: BTreeMap<String, u16>,
    expected_requests: BTreeMap<String, (String, u16)>,
}
impl Fixture {
    async fn start() -> Self {
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts-all-routes.ts");
        let child = Command::new("bun")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Bun is required for actual TypeScript service interoperability");
        let mut child = ChildGuard(child);
        let stdout = child.0.stdout.take().unwrap();
        let line = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
                line
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let announcement: Announcement = serde_json::from_str(&line).unwrap();
        let headers = reqwest::header::HeaderMap::from_iter([
            (
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", announcement.token).parse().unwrap(),
            ),
            (
                reqwest::header::HeaderName::from_static("x-agentkit-operation"),
                "streamRun".parse().unwrap(),
            ),
        ]);
        let client = Client::new(&announcement.base_url)
            .unwrap()
            .with_headers(headers);
        Self {
            _child: child,
            client,
            announcement,
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
        assert_eq!(self.covered.len(), 48);
        let audit: Value = reqwest::Client::new()
            .get(&self.announcement.evidence_url)
            .bearer_auth(&self.announcement.evidence_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut observed = BTreeSet::new();
        let mut observed_cases = BTreeSet::new();
        for request in audit["requests"].as_array().unwrap() {
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
            audit["applied"], 1,
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
fn check(value: &Value, schema: &str) {
    if schema.is_empty() {
        assert!(value.is_null());
    } else if let Some(item) = schema.strip_suffix("[]") {
        for value in value.as_array().unwrap() {
            agentkit_contracts::validate_schema(item, value).unwrap();
        }
    } else {
        agentkit_contracts::validate_schema(schema, value).unwrap();
    }
}
fn id(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap().into()
}

#[tokio::test]
async fn rust_client_exercises_all_48_operations_against_the_actual_typescript_host() {
    let mut fixture = Fixture::start().await;
    let chat = chats(&mut fixture).await;
    providers(&mut fixture).await;
    management(&mut fixture, &chat).await;
    proposals(&mut fixture).await;
    cancellation_and_resume(&mut fixture).await;
    fixture
        .call(Op::DeleteChat, request(&[("chatId", &chat)], None), "", 204)
        .await;
    for op in [Op::ListConnections, Op::SaveConnection, Op::DeleteConnection,
        Op::ConnectionAction, Op::ListPromptTemplates, Op::PublishPromptTemplate,
        Op::DeletePromptTemplate, Op::RenderPromptTemplate, Op::ListSkills] {
        fixture.covered.insert(op.as_str().into(), 501);
        let case = fixture.expected_requests.len().to_string();
        fixture.expected_requests.insert(case.clone(), (op.as_str().into(), 501));
        let mut call = Call::default();
        for segment in op.route().path.split('/') {
            if let Some(name) = segment.strip_prefix(':') {
                call.path.insert(name.into(), "optional-service".into());
            }
        }
        call.headers.insert("x-agentkit-case", case.parse().unwrap());
        call.headers.insert("x-agentkit-operation", op.as_str().parse().unwrap());
        assert!(matches!(fixture.client.call(op, call).await,
            Err(agentkit_client::ClientError::Http { status: 501, .. })));
    }
    fixture.verify().await;
}
