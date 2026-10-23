use agentkit_store::{MessageRecord, ProposalRecord, TaskRecord};
use eframe::egui;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub enum Command {
    Send { content: String, model: String },
    Cancel(String),
    Resume(String),
    Recover,
    Refresh,
    StageNote(String),
    Decide { proposal_id: String, approve: bool },
    Apply(String),
}

pub struct Snapshot {
    pub messages: Vec<MessageRecord>,
    pub tasks: Vec<TaskRecord>,
    pub proposals: Vec<ProposalRecord>,
    pub note: String,
    pub verifications: Vec<VerificationView>,
}

pub struct ConsentPrompt {
    pub app: String,
    pub connection: String,
    pub generation: u64,
    pub account: Option<String>,
    pub destination: String,
    pub model: String,
    pub categories: Vec<String>,
    pub local_demo: bool,
    pub reply: oneshot::Sender<bool>,
}

pub enum Update {
    Provider { models: Vec<String>, label: String },
    Snapshot(Snapshot),
    Event { task_id: String, event: Value },
    Consent(ConsentPrompt),
    Status(String),
    Error(String),
}

#[derive(Clone)]
pub struct UiSender {
    pub channel: std::sync::mpsc::Sender<Update>,
    pub context: egui::Context,
}

impl UiSender {
    pub fn send(&self, update: Update) {
        let _ = self.channel.send(update);
        self.context.request_repaint();
    }
}

pub type Commands = mpsc::Receiver<Command>;

pub struct VerificationView {
    pub task_id: String,
    pub data: Value,
}
