use crate::{backend, bridge::*};
use agentkit_host::{Runtime, SystemRuntime};
use agentkit_store::{ProposalStatus, TaskStatus};
use std::time::Duration;
use tokio::sync::mpsc;

async fn wait_for(
    updates: &std::sync::mpsc::Receiver<Update>,
    predicate: impl Fn(&Update) -> bool,
) -> Update {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(update) = updates.try_recv() {
                match &update {
                    Update::Snapshot(snapshot) => eprintln!(
                        "snapshot tasks={:?} proposals={:?} note={}",
                        snapshot
                            .tasks
                            .iter()
                            .map(|task| task.status)
                            .collect::<Vec<_>>(),
                        snapshot
                            .proposals
                            .iter()
                            .map(|proposal| proposal.status)
                            .collect::<Vec<_>>(),
                        snapshot.note
                    ),
                    Update::Status(status) => eprintln!("status: {status}"),
                    Update::Event { event, .. } => eprintln!("event: {}", event["type"]),
                    Update::Consent(_) => eprintln!("consent"),
                    _ => {}
                }
                if let Update::Error(error) = &update {
                    panic!("backend error: {error}");
                }
                if predicate(&update) {
                    return update;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    })
    .await
    .expect("backend update timeout")
}

#[tokio::test(flavor = "multi_thread")]
async fn native_backend_chat_consent_and_proposal_receipts() {
    let directory = std::env::temp_dir().join(format!("agentkit-egui-test-{}", SystemRuntime.id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("demo.sqlite3");
    let (commands, receiver) = mpsc::channel(32);
    let (sender, updates) = std::sync::mpsc::channel();
    let task = tokio::spawn(backend::run_demo(
        path,
        receiver,
        UiSender {
            channel: sender,
            context: eframe::egui::Context::default(),
        },
    ));
    wait_for(&updates, |update| matches!(update, Update::Snapshot(_))).await;
    commands
        .send(Command::StageNote("Saved with approval".into()))
        .await
        .unwrap();
    let update = wait_for(
        &updates,
        |update| matches!(update, Update::Snapshot(snapshot) if !snapshot.proposals.is_empty()),
    )
    .await;
    let Update::Snapshot(snapshot) = update else {
        unreachable!()
    };
    let proposal_id = snapshot.proposals[0].id.clone();
    commands
        .send(Command::Decide {
            proposal_id: proposal_id.clone(),
            approve: true,
        })
        .await
        .unwrap();
    wait_for(&updates, |update| matches!(update, Update::Snapshot(snapshot) if snapshot.proposals[0].status == ProposalStatus::Approved)).await;
    commands
        .send(Command::Apply(proposal_id.clone()))
        .await
        .unwrap();
    wait_for(&updates, |update| matches!(update, Update::Snapshot(snapshot) if snapshot.note == "Saved with approval")).await;
    commands.send(Command::Apply(proposal_id)).await.unwrap();
    wait_for(&updates, |update| matches!(update, Update::Snapshot(snapshot) if snapshot.proposals[0].status == ProposalStatus::Applied)).await;
    commands
        .send(Command::Send {
            content: "Hello".into(),
            model: "demo-echo".into(),
        })
        .await
        .unwrap();
    let Update::Consent(prompt) =
        wait_for(&updates, |update| matches!(update, Update::Consent(_))).await
    else {
        unreachable!()
    };
    assert!(prompt.local_demo);
    assert_eq!(prompt.categories, ["messages"]);
    prompt.reply.send(true).unwrap();
    let Update::Snapshot(completed) = wait_for(&updates, |update| matches!(update, Update::Snapshot(snapshot) if snapshot.tasks.iter().any(|task| task.status == TaskStatus::Completed))).await else { unreachable!() };
    assert_eq!(completed.verifications.len(), 1);
    assert_eq!(completed.verifications[0].data["status"], "pass");
    assert_eq!(
        completed.verifications[0].data["deficiencies"],
        serde_json::json!([])
    );
    commands
        .send(Command::Send {
            content: "Deny this request".into(),
            model: "demo-echo".into(),
        })
        .await
        .unwrap();
    let Update::Consent(prompt) =
        wait_for(&updates, |update| matches!(update, Update::Consent(_))).await
    else {
        unreachable!()
    };
    prompt.reply.send(false).unwrap();
    wait_for(&updates, |update| matches!(update, Update::Snapshot(snapshot) if snapshot.tasks.iter().any(|task| task.status == TaskStatus::Failed))).await;
    drop(commands);
    task.await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn local_verifier_reports_missing_answers_and_changed_note_state() {
    let store = agentkit_store::SqliteStore::memory(32).unwrap();
    store
        .create_chat("chat", None, serde_json::json!({}), 0)
        .await
        .unwrap();
    let verifier = crate::verification::LocalVerifier::new(store.clone());
    let missing = verifier.inspect("chat", "run").await.unwrap();
    assert_eq!(missing.status, "partial");
    assert!(missing.deficiencies[0].contains("assistant answer"));
    crate::proposals::stage(&store, "chat", "Approved note".into())
        .await
        .unwrap();
    let id = store.proposals(Some("chat".into()), None).await.unwrap()[0]
        .id
        .clone();
    crate::proposals::decide(&store, &id, true).await.unwrap();
    crate::proposals::apply(&store, &id).await.unwrap();
    store
        .set_setting(
            "egui-demo",
            "note",
            serde_json::json!("Changed outside proposal"),
        )
        .await
        .unwrap();
    let changed = verifier.inspect("chat", "run").await.unwrap();
    assert!(
        changed
            .deficiencies
            .iter()
            .any(|deficiency| deficiency.contains("differs"))
    );
}
