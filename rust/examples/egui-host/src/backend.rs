use crate::{bridge::*, proposals, provider, verification};
use agentkit_core::ToolRegistry;
use agentkit_host::{
    AgentHost, ChatExecutor, HostOptions, HostResult, RunUpdate, Runtime, SubmitChat, SystemRuntime,
};
use agentkit_store::SqliteStore;
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

const CHAT_ID: &str = "egui-reference-chat";

pub async fn run(path: PathBuf, mut commands: Commands, ui: UiSender) {
    if let Err(error) = serve(path, &mut commands, &ui, false).await {
        ui.send(Update::Error(error.to_string()));
    }
}

async fn serve(
    path: PathBuf,
    commands: &mut Commands,
    ui: &UiSender,
    offline: bool,
) -> HostResult<()> {
    let host = create_host(path, ui, offline).await?;
    let recovered = host.recover().await?;
    ui.send(Update::Status(format!(
        "Native host ready. {} expired runs recovered; interrupted work needs explicit Resume.",
        recovered.len()
    )));
    refresh(&host, ui).await?;
    let worker = host.start_worker();
    while let Some(command) = commands.recv().await {
        if let Err(error) = dispatch(&host, command, ui).await {
            ui.send(Update::Error(error.to_string()));
        }
        if let Err(error) = refresh(&host, ui).await {
            ui.send(Update::Error(error.to_string()));
        }
    }
    worker.shutdown().await
}

async fn dispatch(host: &AgentHost, command: Command, ui: &UiSender) -> HostResult<()> {
    match command {
        Command::Send { content, model } => {
            let handle = host
                .submit_chat(SubmitChat {
                    task_id: host.runtime().id(),
                    chat_id: CHAT_ID.into(),
                    model,
                    content: content.into(),
                    parent_message_id: None,
                })
                .await?;
            follow(host.clone(), handle.task_id, ui.clone());
        }
        Command::Cancel(task) => host.cancel(&task).await?,
        Command::Resume(task) => {
            host.resume(&task).await?;
            follow(host.clone(), task, ui.clone());
        }
        Command::Recover => {
            let count = host.recover().await?.len();
            ui.send(Update::Status(format!(
                "Recovered {count} expired runs. Interrupted work stays paused until Resume."
            )));
        }
        Command::Refresh => {}
        Command::StageNote(note) => proposals::stage(host.store(), CHAT_ID, note).await?,
        Command::Decide {
            proposal_id,
            approve,
        } => proposals::decide(host.store(), &proposal_id, approve).await?,
        Command::Apply(id) => proposals::apply(host.store(), &id).await?,
    }
    Ok(())
}

fn follow(host: AgentHost, task_id: String, ui: UiSender) {
    tokio::spawn(async move {
        let mut subscription = host.run_handle(&task_id).subscribe(None);
        while let Some(update) = subscription.recv().await {
            match update {
                Ok(RunUpdate::Event(record)) => ui.send(Update::Event {
                    task_id: task_id.clone(),
                    event: record.event,
                }),
                Ok(RunUpdate::Settled(task)) => {
                    ui.send(Update::Status(format!(
                        "Run {}: {:?}",
                        task.task_id, task.status
                    )));
                    break;
                }
                Err(error) => {
                    ui.send(Update::Error(error.to_string()));
                    break;
                }
            }
        }
        if let Err(error) = refresh(&host, &ui).await {
            ui.send(Update::Error(error.to_string()));
        }
    });
}

async fn refresh(host: &AgentHost, ui: &UiSender) -> HostResult<()> {
    let store = host.store();
    let note = store
        .get_setting("egui-demo", "note")
        .await?
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let tasks = store.list_tasks(Some(CHAT_ID.into())).await?;
    let verifications = verification::reports(store, &tasks).await?;
    ui.send(Update::Snapshot(Snapshot {
        messages: store.messages(CHAT_ID, false).await?,
        tasks,
        verifications,
        proposals: store.proposals(Some(CHAT_ID.into()), None).await?,
        note,
    }));
    Ok(())
}

#[cfg(test)]
pub async fn run_demo(path: PathBuf, mut commands: Commands, ui: UiSender) {
    if let Err(error) = serve(path, &mut commands, &ui, true).await {
        ui.send(Update::Error(error.to_string()));
    }
}

async fn create_host(path: PathBuf, ui: &UiSender, offline: bool) -> HostResult<AgentHost> {
    let store = SqliteStore::open(path, 128)?;
    let runtime = Arc::new(SystemRuntime);
    if store.get_chat(CHAT_ID).await?.is_none() {
        store
            .create_chat(
                CHAT_ID,
                Some("Native reference chat".into()),
                json!({}),
                runtime.now_ms(),
            )
            .await?;
    }
    let selection = (if offline {
        provider::offline(ui.clone())
    } else {
        provider::select(ui.clone())
    })
    .map_err(|error| agentkit_host::HostError::Execution(error.to_string()))?;
    pin_provider(&store, selection.binding).await?;
    ui.send(Update::Provider {
        models: selection.models,
        label: selection.label,
    });
    let executor = Arc::new(ChatExecutor::new(
        selection.provider,
        Arc::new(ToolRegistry::default()),
    ));
    let executors: HashMap<String, Arc<dyn agentkit_host::TaskExecutor>> = HashMap::from([(
        "chat.turn".into(),
        executor as Arc<dyn agentkit_host::TaskExecutor>,
    )]);
    let verification = Arc::new(verification::LocalVerifier::new(store.clone()));
    AgentHost::new(
        store,
        executors,
        runtime,
        HostOptions {
            lease_ttl_ms: 3_000,
            verification: Some(verification),
            ..HostOptions::default()
        },
    )
}

async fn pin_provider(store: &SqliteStore, binding: serde_json::Value) -> HostResult<()> {
    let previous = store.get_setting("egui-host", "provider-binding").await?;
    if previous
        .as_ref()
        .is_some_and(|previous| previous != &binding)
        && store
            .list_tasks(None)
            .await?
            .iter()
            .any(|task| !task.status.is_terminal())
    {
        return Err(agentkit_host::HostError::Execution("Provider configuration changed while unfinished runs exist. Reopen with the original provider settings, then cancel or finish those runs before switching.".into()));
    }
    store
        .set_setting("egui-host", "provider-binding", binding)
        .await?;
    Ok(())
}
