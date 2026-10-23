#![allow(dead_code)]
use agentkit_core::{ToolContext, ToolDefinition, ToolLimits};
use agentkit_host::*;
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

pub struct Clock(AtomicUsize);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst) as i64
    }
    fn id(&self) -> String {
        format!("generated-{}", self.0.fetch_add(1, Ordering::SeqCst))
    }
}

pub struct Permission {
    pub allowed: AtomicBool,
    pub apply_checks: AtomicUsize,
    pub revoke_policy_on_claim: bool,
    pub policy: Arc<SessionWritePolicy>,
}

#[async_trait]
impl Authorization for Permission {
    async fn authorize(&self, _actor: &Value, action: &str, _resource: &Value) -> HostResult<()> {
        if action == "apply"
            && self.revoke_policy_on_claim
            && self.apply_checks.fetch_add(1, Ordering::SeqCst) >= 1
        {
            self.policy.set_mode(WritePolicyMode::ConfirmAllWrites)?;
        }
        if self.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(HostError::Execution("private-authorizer-detail".into()))
        }
    }
}

pub struct Domain {
    pub calls: AtomicUsize,
    pub ambiguous: AtomicBool,
    pub status: Mutex<String>,
    pub receipts: Mutex<BTreeMap<String, (String, Value)>>,
    pub execution_fences: Mutex<Vec<Option<agentkit_core::DispatchFence>>>,
}

#[async_trait]
impl ProposalApplier for Domain {
    async fn apply(&self, identity: MutationIdentity, operations: Value) -> HostResult<Value> {
        self.execution_fences
            .lock()
            .unwrap()
            .push(identity.execution_fence.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        let status = self.status.lock().unwrap().clone();
        let failures = if status == "partial" {
            vec![json!({"opIndex":1,"error":"item no longer exists"})]
        } else {
            vec![]
        };
        let result = json!({"status":status,"appliedOps":operations.as_array().unwrap().len().min(1),"failedOps":failures,"resultJson":null,"revision":"2"});
        self.receipts.lock().unwrap().insert(
            identity.operation_id,
            (identity.fingerprint, result.clone()),
        );
        if self.ambiguous.load(Ordering::SeqCst) {
            Err(HostError::Execution("private-domain-detail".into()))
        } else {
            Ok(result)
        }
    }
    async fn reconcile(&self, identity: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(self
            .receipts
            .lock()
            .unwrap()
            .get(&identity.operation_id)
            .filter(|(fingerprint, _)| *fingerprint == identity.fingerprint)
            .map(|(_, value)| value.clone()))
    }
}

pub struct Builder {
    pub revision: Mutex<Option<String>>,
    pub last_input: Mutex<Option<Value>>,
}

#[async_trait]
impl ProposalBuilder for Builder {
    async fn build(&self, _context: &ToolContext, input: &Value) -> HostResult<ProposalBuild> {
        *self.last_input.lock().unwrap() = Some(input.clone());
        Ok(ProposalBuild {
            scope_key: input["scope"].as_str().unwrap_or("document").into(),
            kind: "document.edits".into(),
            risk: if input["destructive"] == true {
                WriteRisk::Destructive
            } else {
                WriteRisk::Low
            },
            operations: if input["empty"] == true {
                vec![]
            } else if input["two_operations"] == true {
                vec![json!({"change":1}), json!({"change":2})]
            } else {
                vec![json!({"change":input["change"]})]
            },
            warnings: if input["warning"] == true {
                vec!["Target needs review".into()]
            } else {
                vec![]
            },
            truncated: input["truncated"] == true,
            envelope: json!({"preview":"document change"}),
            revision_at_create: Some("1".into()),
        })
    }
    async fn current_revision(&self, _scope: &str) -> HostResult<Option<String>> {
        Ok(self.revision.lock().unwrap().clone())
    }
}

pub struct Fixture {
    pub store: SqliteStore,
    pub service: Arc<ProposalService>,
    pub tool: Arc<GovernedProposalTool>,
    pub policy: Arc<SessionWritePolicy>,
    pub domain: Arc<Domain>,
    pub permission: Arc<Permission>,
    pub builder: Arc<Builder>,
}

pub async fn fixture(mode: WriteExecutionMode, revoke_policy_on_claim: bool) -> Fixture {
    fixture_with_store(
        mode,
        revoke_policy_on_claim,
        SqliteStore::memory(32).unwrap(),
    )
    .await
}

pub async fn fixture_with_store(
    mode: WriteExecutionMode,
    revoke_policy_on_claim: bool,
    store: SqliteStore,
) -> Fixture {
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let policy = Arc::new(SessionWritePolicy::new(WritePolicyMode::AutoAll));
    let permission = permission(policy.clone(), revoke_policy_on_claim);
    let domain = domain();
    let runtime = Arc::new(Clock(AtomicUsize::new(1)));
    let service = Arc::new(ProposalService::new(
        store.clone(),
        domain.clone(),
        permission.clone(),
        runtime.clone(),
    ));
    let builder = Arc::new(Builder {
        revision: Mutex::new(Some("1".into())),
        last_input: Mutex::new(None),
    });
    let tool = governed_tool(ProposalToolOptions {
        store: store.clone(),
        service: service.clone(),
        builder: builder.clone(),
        authorization: permission.clone(),
        policy: policy.clone(),
        runtime,
        actor: json!({"id":"trusted-user"}),
        actor_id: None,
        mode,
        policy_id: "session-write-policy".into(),
    });
    Fixture {
        store,
        service,
        tool,
        policy,
        domain,
        permission,
        builder,
    }
}

fn governed_tool(options: ProposalToolOptions) -> Arc<GovernedProposalTool> {
    Arc::new(GovernedProposalTool::new(definition("write_document", "write"), options).unwrap())
}

fn permission(policy: Arc<SessionWritePolicy>, revoke_policy_on_claim: bool) -> Arc<Permission> {
    Arc::new(Permission {
        allowed: AtomicBool::new(true),
        apply_checks: AtomicUsize::new(0),
        revoke_policy_on_claim,
        policy,
    })
}

fn domain() -> Arc<Domain> {
    Arc::new(Domain {
        calls: AtomicUsize::new(0),
        ambiguous: AtomicBool::new(false),
        status: Mutex::new("applied".into()),
        receipts: Mutex::new(BTreeMap::new()),
        execution_fences: Mutex::new(vec![]),
    })
}

pub fn definition(name: &str, effect: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1".into(),
        effect: effect.into(),
        capability: "document".into(),
        description: if effect == "write" {
            "Write document edits through durable proposals."
        } else {
            "Read document."
        }
        .into(),
        input_schema: json!({"type":"object"}),
        output_schema: None,
        timeout_ms: Some(1000),
    }
}

pub fn context() -> ToolContext {
    ToolContext {
        run_id: "run".into(),
        chat_id: Some("chat".into()),
        scope_id: Some("document".into()),
        user_id: Some("trusted-user".into()),
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        dispatch_guard: None,
        metadata: Default::default(),
    }
}

pub fn contribution() -> ToolContributionContext {
    ToolContributionContext {
        chat_id: Some("chat".into()),
        run_id: "run".into(),
        scope_id: Some("document".into()),
        actor: json!({"id":"trusted-user"}),
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: true,
        timeout_ms: 20,
    }
}

pub fn query() -> WriteQuery {
    WriteQuery {
        chat_id: "chat".into(),
        tool_name: "write_document".into(),
        proposal_kind: "document.edits".into(),
        scope_key: "document".into(),
        risk: WriteRisk::Low,
        actor_id: None,
        payload_fingerprint: "payload".into(),
        revision: Some("1".into()),
    }
}

pub fn grant(query: &WriteQuery) -> WriteAllowance {
    WriteAllowance {
        chat_id: query.chat_id.clone(),
        tool_name: query.tool_name.clone(),
        proposal_kind: query.proposal_kind.clone(),
        scope_key: Some(query.scope_key.clone()),
        max_risk: query.risk,
        actor_id: query.actor_id.clone(),
        payload_fingerprint: query
            .actor_id
            .as_ref()
            .map(|_| query.payload_fingerprint.clone()),
        revision: query.revision.clone(),
    }
}
