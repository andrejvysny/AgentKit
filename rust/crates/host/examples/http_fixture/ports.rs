use agentkit_contracts::*;
use agentkit_host::http::{ProviderOperations, ToolCatalogPort, WritePolicyPort};
use agentkit_host::{Authorization, HostResult, MutationIdentity, ProposalApplier};
use agentkit_store::{ProposalRecord, ProposalStatus, SqliteStore};
use agentkit_transport_http::{Actor, Problem};
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub struct FixturePorts {
    pub store: SqliteStore,
}
fn error(error: impl std::fmt::Display) -> Problem {
    Problem::host("internal_error", error.to_string())
}
fn typed<T: DeserializeOwned>(value: Value) -> Result<T, Problem> {
    serde_json::from_value(value).map_err(error)
}
fn encoded(value: impl Serialize) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(error)
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[async_trait]
impl WritePolicyPort for FixturePorts {
    async fn list(&self, _: &Actor, chat: &str) -> Result<Vec<WriteAllowanceDto>, Problem> {
        self.store
            .list_settings(&format!("allowance:{chat}"))
            .await
            .map_err(error)?
            .into_iter()
            .map(|(_, v)| typed(v))
            .collect()
    }
    async fn grant(
        &self,
        _: &Actor,
        chat: &str,
        input: GrantAllowanceRequest,
    ) -> Result<WriteAllowanceDto, Problem> {
        let input = encoded(input)?;
        let key = format!(
            "{}:{}",
            input["toolName"].as_str().unwrap(),
            input["proposalKind"].as_str().unwrap()
        );
        let value = json!({"key":key,"chatId":chat,"toolName":input["toolName"],"proposalKind":input["proposalKind"],"maxRisk":input["maxRisk"],"createdAt":now()});
        self.store
            .set_setting(&format!("allowance:{chat}"), &key, value.clone())
            .await
            .map_err(error)?;
        typed(value)
    }
    async fn revoke(&self, _: &Actor, chat: &str, key: &str) -> Result<(), Problem> {
        self.store
            .delete_setting(&format!("allowance:{chat}"), key)
            .await
            .map_err(error)?;
        Ok(())
    }
}
#[async_trait]
impl ToolCatalogPort for FixturePorts {
    async fn list(&self, _: &Actor) -> Result<Vec<AiToolDefinition>, Problem> {
        Ok(vec![])
    }
}
#[async_trait]
impl ProviderOperations for FixturePorts {
    async fn refresh_models(&self, _: &Actor, id: &str) -> Result<Vec<AiProviderModel>, Problem> {
        Ok(vec![typed(
            json!({"providerId":id,"modelId":"m1","displayName":"Fixture model","fetchedAt":now()}),
        )?])
    }
    async fn test(&self, _: &Actor, _: &str) -> Result<TestProviderResponse, Problem> {
        typed(json!({"ok":true,"latencyMs":1}))
    }
}
#[async_trait]
impl Authorization for FixturePorts {
    async fn authorize(&self, _: &Value, _: &str, _: &Value) -> HostResult<()> {
        Ok(())
    }
}
#[async_trait]
impl ProposalApplier for FixturePorts {
    async fn apply(&self, _: MutationIdentity, _: Value) -> HostResult<Value> {
        Ok(
            json!({"status":"applied","appliedOps":1,"failedOps":[],"resultJson":null,"revision":null}),
        )
    }
    async fn reconcile(&self, _: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(None)
    }
}
pub async fn seed(store: &SqliteStore) -> HostResult<()> {
    if store.get_chat("chat-client").await?.is_none() {
        store.create_chat("chat-client", None, json!({}), 0).await?;
    }
    if store.get_task("resume-me").await?.is_none() {
        store.create_chat("resume-chat", None, json!({}), 0).await?;
        store
            .submit(agentkit_store::SubmitTask {
                task_id: "resume-me".into(),
                kind: "chat.turn".into(),
                scope_id: "resume-chat".into(),
                payload: json!({"chatId":"resume-chat","model":"m1"}),
                priority: 0,
                now: 0,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await?;
        store
            .claim(agentkit_store::ClaimInput {
                owner_id: "crashed-fixture".into(),
                now: 0,
                ttl_ms: 3,
                kinds: vec!["chat.turn".into()],
                busy_scopes: vec![],
                max_attempts: 3,
            })
            .await?;
        store
            .recover(4, 3, 0, agentkit_store::RecoveryMode::Manual)
            .await?;
    }
    for id in ["approve-me", "reject-me"] {
        if store.get_proposal(id).await?.is_some() {
            continue;
        }
        store
            .create_proposal(ProposalRecord {
                id: id.into(),
                chat_id: "chat-client".into(),
                run_id: None,
                scope_key: "chat-client".into(),
                action_id: None,
                tool_name: "edit".into(),
                kind: "edit".into(),
                risk: "low".into(),
                status: ProposalStatus::Pending,
                envelope: json!({"summary":"Fixture proposal"}),
                operations: vec![json!({"type":"edit"})],
                warnings: vec![],
                truncated: false,
                revision_at_create: None,
                operation_id: None,
                decision: None,
                reason: None,
                created_at: 0,
                decided_at: None,
                claimed_at: None,
                applied_at: None,
            })
            .await?;
    }
    Ok(())
}

pub async fn seed_compat(
    store: &SqliteStore,
    backend: &agentkit_host::http::DesktopBackend,
) -> Result<(), Problem> {
    use agentkit_transport_http::{Backend, OperationRequest};
    let actor = Actor {
        id: "fixture-actor".into(),
        metadata: Default::default(),
    };
    let request = |operation: &str, body: Value| OperationRequest {
        actor: actor.clone(),
        operation: operation.into(),
        params: Default::default(),
        query: Default::default(),
        body,
        idempotency: None,
    };
    let providers = backend
        .dispatch(request("listProviders", Value::Null))
        .await?;
    if providers.body.as_array().unwrap().is_empty() {
        backend.dispatch(request("createProvider",json!({"id":"p1","label":"Mock","kind":"openai-compatible","baseUrl":"http://localhost:1234","defaultModel":"m1"}))).await?;
    }
    if store
        .get_setting("http", "settings")
        .await
        .map_err(error)?
        .is_none()
    {
        backend
            .dispatch(request("updateSettings", json!({"defaultProviderId":"p1"})))
            .await?;
    }
    store.set_setting("http.provider-catalog","p1",json!([{"providerId":"p1","modelId":"m1","displayName":"Mock model","fetchedAt":"1970-01-01T00:00:00.000Z"}])).await.map_err(error)?;
    if store.get_chat("chat-react").await.map_err(error)?.is_none() {
        store
            .create_chat("chat-react", None, json!({}), 0)
            .await
            .map_err(error)?;
    }
    Ok(())
}
