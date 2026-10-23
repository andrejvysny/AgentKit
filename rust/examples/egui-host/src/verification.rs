use crate::bridge::VerificationView;
use agentkit_host::{ExecutionContext, HostResult, VerificationHook, VerificationReport};
use agentkit_store::{ProposalStatus, SqliteStore, TaskRecord};
use async_trait::async_trait;

/// Checks local persisted state only; it does not judge the provider answer's truth.
pub struct LocalVerifier {
    store: SqliteStore,
}

impl LocalVerifier {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub async fn inspect(&self, chat_id: &str, run_id: &str) -> HostResult<VerificationReport> {
        let messages = self.store.messages(chat_id, true).await?;
        let has_answer = messages.iter().any(|message| {
            message.run_id.as_deref() == Some(run_id)
                && message.role == "assistant"
                && message.metadata["placeholder"] != true
                && message.metadata["internal"] != true
                && message
                    .content
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty())
        });
        let mut deficiencies = Vec::new();
        if !has_answer {
            deficiencies.push(
                "No nonempty completed assistant answer was found in the durable conversation."
                    .into(),
            );
        }
        self.check_note(chat_id, &mut deficiencies).await?;
        Ok(VerificationReport {
            status: if deficiencies.is_empty() {
                "pass"
            } else {
                "partial"
            }
            .into(),
            deficiencies,
        })
    }

    async fn check_note(&self, chat_id: &str, deficiencies: &mut Vec<String>) -> HostResult<()> {
        let proposals = self
            .store
            .proposals(Some(chat_id.into()), Some(ProposalStatus::Applied))
            .await?;
        let latest = proposals
            .iter()
            .filter(|proposal| proposal.kind == "demo.note")
            .max_by_key(|proposal| proposal.applied_at);
        let Some(proposal) = latest else {
            return Ok(());
        };
        let receipt = if let Some(operation) = &proposal.operation_id {
            self.store.receipt(operation).await?
        } else {
            None
        };
        if receipt.as_ref().is_none_or(|receipt| {
            receipt.proposal_id != proposal.id || receipt.outcome.status != "applied"
        }) {
            deficiencies
                .push("The latest applied demo note has no matching successful receipt.".into());
        }
        let note = self.store.get_setting("egui-demo", "note").await?;
        if note.as_ref() != proposal.envelope.get("note") {
            deficiencies
                .push("The saved demo note differs from the latest applied proposal.".into());
        }
        Ok(())
    }
}

#[async_trait]
impl VerificationHook for LocalVerifier {
    async fn verify(&self, context: ExecutionContext) -> HostResult<VerificationReport> {
        let chat_id = context.task.payload["chatId"].as_str().unwrap_or("");
        self.inspect(chat_id, &context.task.task_id).await
    }
}

pub async fn reports(
    store: &SqliteStore,
    tasks: &[TaskRecord],
) -> HostResult<Vec<VerificationView>> {
    let mut reports = Vec::new();
    for task in tasks {
        let next = store.next_seq(&task.task_id).await?;
        let after = (next > 64).then(|| next - 65);
        for event in store.events(&task.task_id, after, 64).await? {
            if event.event["type"] == "run.verification" {
                reports.push(VerificationView {
                    task_id: task.task_id.clone(),
                    data: event.event["data"].clone(),
                });
            }
        }
    }
    Ok(reports)
}
