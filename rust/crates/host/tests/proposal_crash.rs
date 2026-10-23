use agentkit_host::*;
use agentkit_store::{ProposalRecord, ProposalStatus, SqliteStore};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::Arc};

struct Permit;
#[async_trait]
impl Authorization for Permit {
    async fn authorize(&self, _: &Value, _: &str, _: &Value) -> HostResult<()> {
        Ok(())
    }
}

struct DurableDomain {
    ledger: SqliteStore,
    crash: Option<String>,
}
#[async_trait]
impl ProposalApplier for DurableDomain {
    async fn apply(&self, identity: MutationIdentity, _: Value) -> HostResult<Value> {
        assert_eq!(
            self.crash.as_deref(),
            Some("apply"),
            "recovery must never repeat the mutation"
        );
        let receipt = json!({"proposalId":identity.proposal_id,"fingerprint":identity.fingerprint,
            "outcome":{"status":"applied","appliedOps":1,"failedOps":[],"revision":"2","resultJson":null}});
        assert!(
            self.ledger
                .bind_setting("domain-receipt", &identity.operation_id, receipt)
                .await?
        );
        // Immediate process termination skips all Rust destructors after durable commit.
        std::process::exit(72);
    }
    async fn reconcile(&self, identity: MutationIdentity) -> HostResult<Option<Value>> {
        let receipt = self
            .ledger
            .get_setting("domain-receipt", &identity.operation_id)
            .await?;
        let Some(receipt) = receipt else {
            return Ok(None);
        };
        assert_eq!(receipt["proposalId"], identity.proposal_id);
        assert_eq!(receipt["fingerprint"], identity.fingerprint);
        if self.crash.as_deref() == Some("reconcile") {
            std::process::exit(73);
        }
        Ok(Some(receipt["outcome"].clone()))
    }
}

fn service(directory: &Path, crash: Option<String>) -> (SqliteStore, ProposalService) {
    let store = SqliteStore::open(directory.join("host.sqlite"), 16).unwrap();
    let ledger = SqliteStore::open(directory.join("domain.sqlite"), 16).unwrap();
    let service = ProposalService::new(
        store.clone(),
        Arc::new(DurableDomain { ledger, crash }),
        Arc::new(Permit),
        Arc::new(SystemRuntime),
    );
    (store, service)
}

#[tokio::test]
async fn proposal_crash_child() {
    let Ok(directory) = std::env::var("AGENTKIT_PROPOSAL_CRASH_DIR") else {
        return;
    };
    let mode = std::env::var("AGENTKIT_PROPOSAL_CRASH_MODE").unwrap();
    let (store, service) = service(Path::new(&directory), Some(mode.clone()));
    if mode == "apply" {
        store.create_chat("chat", None, json!({}), 0).await.unwrap();
        let proposal: ProposalRecord = serde_json::from_value(json!({"id":"proposal","chatId":"chat",
            "scopeKey":"document","toolName":"write","kind":"edit","risk":"low","status":"pending",
            "envelope":{},"operations":[{"edit":1}],"warnings":[],"truncated":false,"revisionAtCreate":"1","createdAt":0})).unwrap();
        store.create_proposal(proposal).await.unwrap();
        service.approve("proposal", &json!("actor")).await.unwrap();
        service
            .apply("proposal", "operation", &json!("actor"))
            .await
            .unwrap();
    } else {
        service.reconcile().await.unwrap();
    }
    panic!("child did not reach its crash boundary");
}

#[tokio::test]
async fn committed_effect_survives_repeated_crashes_before_host_publication() {
    let directory =
        std::env::temp_dir().join(format!("agentkit-proposal-crash-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    for (mode, code) in [("apply", 72), ("reconcile", 73), ("reconcile", 73)] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proposal_crash_child", "--nocapture"])
            .env("AGENTKIT_PROPOSAL_CRASH_DIR", &directory)
            .env("AGENTKIT_PROPOSAL_CRASH_MODE", mode)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (store, _) = service(&directory, None);
        assert_eq!(
            store
                .get_proposal("proposal")
                .await
                .unwrap()
                .unwrap()
                .status,
            ProposalStatus::Applying
        );
        assert!(store.receipt("operation").await.unwrap().is_none());
    }
    {
        let (store, service) = service(&directory, None);
        assert_eq!(service.reconcile().await.unwrap().len(), 1);
        assert!(service.reconcile().await.unwrap().is_empty());
        let receipt = service
            .apply("proposal", "operation", &json!("actor"))
            .await
            .unwrap();
        assert_eq!(receipt.outcome.applied_ops, 1);
        assert_eq!(
            store
                .get_proposal("proposal")
                .await
                .unwrap()
                .unwrap()
                .status,
            ProposalStatus::Applied
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
