use super::support::*;
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolRegistry, ToolResult};
use agentkit_host::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct ReadTool {
    definition: ToolDefinition,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _input: Value) -> Result<ToolResult, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"read":true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}

struct Contributor {
    name: String,
    tools: Vec<ContributedTool>,
    unbound: Option<BTreeSet<String>>,
    fail: bool,
    hang: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        &self.name
    }
    fn unbound_tool_names(&self) -> Option<BTreeSet<String>> {
        self.unbound.clone()
    }
    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hang {
            std::future::pending::<()>().await;
        }
        if self.fail {
            return Err(HostError::Execution("private-contributor-detail".into()));
        }
        Ok(self.tools.clone())
    }
}

fn contributor(name: &str, tools: Vec<ContributedTool>) -> Contributor {
    Contributor {
        name: name.into(),
        tools,
        unbound: None,
        fail: false,
        hang: false,
        calls: AtomicUsize::new(0),
    }
}

fn read(name: &str, calls: &Arc<AtomicUsize>) -> ContributedTool {
    ContributedTool::Read(Arc::new(ReadTool {
        definition: definition(name, "read"),
        calls: calls.clone(),
    }))
}

struct Guard {
    visible: bool,
    allowed: Arc<AtomicBool>,
    broken: bool,
}

#[async_trait]
impl ToolGuard for Guard {
    async fn is_visible(&self, _context: &ToolGuardContext) -> HostResult<bool> {
        Ok(self.visible)
    }
    async fn can_execute(&self, _context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        if self.broken {
            return Err(HostError::Execution("private-guard-detail".into()));
        }
        Ok(if self.allowed.load(Ordering::SeqCst) {
            ToolGuardVerdict::Allowed
        } else {
            ToolGuardVerdict::Refused("Permission was revoked".into())
        })
    }
}

#[tokio::test]
async fn namespaces_are_checked_before_contributors_and_collisions_fail_closed() {
    let good = Arc::new(contributor("document", vec![]));
    for namespace in ["MCP", "mcp", "chat", "agentkit", "bad.name", " space"] {
        let invalid = Arc::new(contributor(namespace, vec![]));
        assert!(
            stage_tools(&[good.clone(), invalid], &[], contribution())
                .await
                .is_err()
        );
    }
    assert_eq!(good.calls.load(Ordering::SeqCst), 0);
    let calls = Arc::new(AtomicUsize::new(0));
    let one = Arc::new(contributor("one", vec![read("read_document", &calls)]));
    let two = Arc::new(contributor("two", vec![read("read_document", &calls)]));
    assert!(stage_tools(&[one, two], &[], contribution()).await.is_err());
}

#[tokio::test]
async fn contributor_failures_and_timeouts_leave_other_tools_available() {
    let calls = Arc::new(AtomicUsize::new(0));
    let good = Arc::new(contributor("document", vec![read("read_document", &calls)]));
    let mut failure = contributor("failed", vec![]);
    failure.fail = true;
    let mut timeout = contributor("timeout", vec![]);
    timeout.hang = true;
    let staged = stage_tools(
        &[good, Arc::new(failure), Arc::new(timeout)],
        &[],
        contribution(),
    )
    .await
    .unwrap();
    assert_eq!(staged.registry.len(), 1);
    assert_eq!(staged.failed.len(), 2);
    assert_eq!(staged.failed[0].reason, "contributor error");
    assert!(staged.failed[1].timed_out);
}

#[tokio::test]
async fn unbound_pruning_matches_declared_union_and_duplicates_stay_per_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut contributor = contributor(
        "document",
        vec![
            read("read_document", &calls),
            read("read_document", &calls),
            read("target_only", &calls),
        ],
    );
    contributor.unbound = Some(BTreeSet::from(["read_document".into()]));
    let mut context = contribution();
    context.has_primary_binding = false;
    let staged = stage_tools(&[Arc::new(contributor)], &[], context)
        .await
        .unwrap();
    assert_eq!(staged.registry.len(), 1);
    assert_eq!(staged.skipped, vec!["read_document"]);
    assert!(staged.registry.get("target_only").is_none());
}

#[tokio::test]
async fn guard_visibility_and_runtime_revocation_preserve_guard_envelope() {
    let calls = Arc::new(AtomicUsize::new(0));
    let contributor: Arc<dyn ToolContributor> =
        Arc::new(contributor("document", vec![read("read_document", &calls)]));
    let allowed = Arc::new(AtomicBool::new(true));
    let hidden: Arc<dyn ToolGuard> = Arc::new(Guard {
        visible: false,
        allowed: allowed.clone(),
        broken: false,
    });
    assert!(
        stage_tools(
            std::slice::from_ref(&contributor),
            &[hidden],
            contribution()
        )
        .await
        .unwrap()
        .registry
        .is_empty()
    );
    let guard: Arc<dyn ToolGuard> = Arc::new(Guard {
        visible: true,
        allowed: allowed.clone(),
        broken: false,
    });
    let staged = stage_tools(&[contributor], &[guard], contribution())
        .await
        .unwrap();
    allowed.store(false, Ordering::SeqCst);
    let result = staged
        .registry
        .get("read_document")
        .unwrap()
        .execute(context(), json!({}))
        .await
        .unwrap();
    assert!(!result.ok);
    assert_eq!(
        result.model_data.as_ref().unwrap()["errorCode"],
        "tool_guard_refused"
    );
    assert_eq!(result.model_data.as_ref().unwrap()["phase"], "guard");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn thrown_guard_errors_and_context_mismatch_never_dispatch_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let contributor: Arc<dyn ToolContributor> =
        Arc::new(contributor("document", vec![read("read_document", &calls)]));
    let guard: Arc<dyn ToolGuard> = Arc::new(Guard {
        visible: true,
        allowed: Arc::new(AtomicBool::new(true)),
        broken: true,
    });
    let staged = stage_tools(&[contributor], &[guard], contribution())
        .await
        .unwrap();
    let tool = staged.registry.get("read_document").unwrap();
    let result = tool.execute(context(), json!({})).await.unwrap();
    assert_eq!(result.summary.as_deref(), Some("guard error"));
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("private-guard-detail")
    );
    let mut other = context();
    other.chat_id = Some("other-chat".into());
    assert!(!tool.execute(other, json!({})).await.unwrap().ok);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn raw_writes_are_not_read_tools_and_governed_actor_must_match() {
    let calls = Arc::new(AtomicUsize::new(0));
    let raw = Arc::new(ReadTool {
        definition: definition("raw_write", "write"),
        calls,
    });
    let contributor = Arc::new(contributor("document", vec![ContributedTool::Read(raw)]));
    assert!(
        stage_tools(&[contributor], &[], contribution())
            .await
            .is_err()
    );
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    let contributor = Arc::new(self::contributor(
        "document",
        vec![ContributedTool::Write(fixture.tool)],
    ));
    let mut different = contribution();
    different.actor = json!({"id":"another-user"});
    assert!(stage_tools(&[contributor], &[], different).await.is_err());
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn static_adapter_preserves_direct_writes_unless_proposal_only_is_selected() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(ReadTool {
            definition: definition("read_document", "read"),
            calls: calls.clone(),
        }))
        .unwrap();
    registry
        .register(Arc::new(ReadTool {
            definition: definition("raw_write", "write"),
            calls,
        }))
        .unwrap();
    let registry = Arc::new(registry);
    let contributor = StaticToolContributor::new(registry.clone());
    assert!(contributor.skipped_write_names().is_empty());
    let staged = stage_tools(&[Arc::new(contributor)], &[], contribution())
        .await
        .unwrap();
    assert_eq!(staged.registry.len(), 2);
    assert!(staged.registry.get("read_document").is_some());
    assert!(
        staged
            .registry
            .get("raw_write")
            .unwrap()
            .definition()
            .description
            .starts_with("Direct write tool")
    );
    let contributor = StaticToolContributor::proposal_only(registry);
    assert_eq!(contributor.skipped_write_names(), vec!["raw_write"]);
    let staged = stage_tools(&[Arc::new(contributor)], &[], contribution())
        .await
        .unwrap();
    assert_eq!(staged.registry.len(), 1);
}

#[tokio::test]
async fn explicit_direct_writes_follow_same_dynamic_authorization_guards() {
    let calls = Arc::new(AtomicUsize::new(0));
    let raw = Arc::new(ReadTool {
        definition: definition("direct_write", "write"),
        calls: calls.clone(),
    });
    let contributor = Arc::new(contributor(
        "document",
        vec![ContributedTool::DirectWrite(raw)],
    ));
    let allowed = Arc::new(AtomicBool::new(true));
    let guard: Arc<dyn ToolGuard> = Arc::new(Guard {
        visible: true,
        allowed: allowed.clone(),
        broken: false,
    });
    let staged = stage_tools(&[contributor], &[guard], contribution())
        .await
        .unwrap();
    let tool = staged.registry.get("direct_write").unwrap();
    assert!(tool.execute(context(), json!({})).await.unwrap().ok);
    allowed.store(false, Ordering::SeqCst);
    assert!(!tool.execute(context(), json!({})).await.unwrap().ok);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
