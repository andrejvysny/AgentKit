use crate::{backend, bridge::*};
use agentkit_client::RunState;
use agentkit_store::{ProposalStatus, TaskStatus};
use eframe::egui;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    time::Duration,
};
use tokio::{runtime::Runtime, sync::mpsc};

pub struct NativeApp {
    runtime: Option<Runtime>,
    commands: mpsc::Sender<Command>,
    updates: std::sync::mpsc::Receiver<Update>,
    snapshot: Snapshot,
    prompts: VecDeque<ConsentPrompt>,
    runs: BTreeMap<String, RunState>,
    prompt: String,
    note: String,
    model: String,
    models: Vec<String>,
    provider_label: String,
    status: String,
    error: Option<String>,
    database: PathBuf,
}

impl NativeApp {
    pub fn new(
        context: &eframe::CreationContext<'_>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let runtime = Runtime::new()?;
        let (commands, receiver) = mpsc::channel(32);
        let (sender, updates) = std::sync::mpsc::channel();
        let database = std::env::var_os("AGENTKIT_DEMO_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("agentkit-egui-demo.sqlite3"));
        runtime.spawn(backend::run(
            database.clone(),
            receiver,
            UiSender {
                channel: sender,
                context: context.egui_ctx.clone(),
            },
        ));
        Ok(Self {
            runtime: Some(runtime),
            commands,
            updates,
            database,
            snapshot: Snapshot {
                messages: vec![],
                tasks: vec![],
                proposals: vec![],
                note: String::new(),
                verifications: vec![],
            },
            prompts: VecDeque::new(),
            runs: BTreeMap::new(),
            prompt: String::new(),
            note: String::new(),
            model: "demo-echo".into(),
            models: vec!["demo-echo".into(), "demo-slow".into()],
            provider_label: "Loading provider…".into(),
            status: "Opening local store…".into(),
            error: None,
        })
    }

    fn command(&mut self, command: Command) {
        if self.commands.try_send(command).is_err() {
            self.error = Some("Background command queue unavailable; retry after refresh.".into());
        }
    }

    fn receive(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            match update {
                Update::Provider { models, label } => {
                    self.model = models.first().cloned().unwrap_or_default();
                    self.models = models;
                    self.provider_label = label;
                }
                Update::Snapshot(snapshot) => {
                    for task in &snapshot.tasks {
                        let state = self.runs.entry(task.task_id.clone()).or_default();
                        state.status = serde_json::to_value(task.status)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned));
                    }
                    self.snapshot = snapshot;
                }
                Update::Consent(prompt) => self.prompts.push_back(prompt),
                Update::Status(status) => self.status = status,
                Update::Error(error) => self.error = Some(error),
                Update::Event { task_id, event } => self.event(task_id, event),
            }
        }
        self.prompts.retain(|prompt| !prompt.reply.is_closed());
    }

    fn event(&mut self, task: String, event: serde_json::Value) {
        match self.runs.entry(task.clone()).or_default().observe(&event) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        }
        match event["type"].as_str() {
            Some("run.verification") => {
                self.snapshot.verifications.push(VerificationView {
                    task_id: task,
                    data: event["data"].clone(),
                });
            }
            Some("run.warning") => {
                self.status = event["data"]["message"]
                    .as_str()
                    .unwrap_or("Provider warning")
                    .into()
            }
            _ => {}
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.heading("AgentKit Native Reference");
        ui.label(&self.provider_label);
        ui.small("Real SQLite store, durable runner, connection consent, proposal receipts");
        ui.small(format!("Database: {}", self.database.display()));
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Model")
                .selected_text(&self.model)
                .show_ui(ui, |ui| {
                    for model in &self.models {
                        ui.selectable_value(&mut self.model, model.clone(), model);
                    }
                });
            if ui.button("Refresh").clicked() {
                self.command(Command::Refresh);
            }
            if ui.button("Scan expired runs").clicked() {
                self.command(Command::Recover);
            }
        });
        ui.label(&self.status);
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        if self.error.is_some() && ui.button("Dismiss error").clicked() {
            self.error = None;
        }
    }

    fn chat(&mut self, ui: &mut egui::Ui) {
        ui.heading("Chat");
        egui::ScrollArea::vertical()
            .id_salt("messages")
            .max_height(250.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for message in &self.snapshot.messages {
                    if message.metadata["placeholder"] == true {
                        continue;
                    }
                    ui.strong(&message.role);
                    ui.label(
                        message
                            .content
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| message.content.to_string()),
                    );
                    ui.separator();
                }
                for state in self.runs.values() {
                    if !matches!(
                        state.status.as_deref(),
                        Some("completed" | "failed" | "cancelled")
                    ) && !state.text.is_empty()
                    {
                        ui.label(&state.text);
                        ui.small(format!("{:?}", state.phase()));
                    }
                }
            });
        ui.add(
            egui::TextEdit::multiline(&mut self.prompt)
                .hint_text("Message the selected provider")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
        if ui
            .add_enabled(!self.prompt.trim().is_empty(), egui::Button::new("Send"))
            .clicked()
        {
            let content = std::mem::take(&mut self.prompt);
            self.command(Command::Send {
                content,
                model: self.model.clone(),
            });
        }
    }

    fn tasks(&mut self, ui: &mut egui::Ui) {
        ui.heading("Runs and restart recovery");
        ui.small("Closing this app stops its worker. Reopen the same database; after the 3 s lease expires, scan and explicitly resume interrupted runs.");
        let mut commands = vec![];
        for task in &self.snapshot.tasks {
            ui.horizontal(|ui| {
                ui.monospace(&task.task_id);
                ui.label(format!(
                    "{:?} · attempt {}",
                    task.status, task.attempt_count
                ));
                if matches!(task.status, TaskStatus::Queued | TaskStatus::Running)
                    && ui.button("Cancel").clicked()
                {
                    commands.push(Command::Cancel(task.task_id.clone()));
                }
                if task.status == TaskStatus::Interrupted && ui.button("Resume").clicked() {
                    commands.push(Command::Resume(task.task_id.clone()));
                }
            });
            if let Some(error) = &task.error {
                ui.small(error);
            }
        }
        for command in commands {
            self.command(command);
        }
    }

    fn proposals(&mut self, ui: &mut egui::Ui) {
        ui.heading("Local note proposals");
        ui.label(format!("Saved note: {}", self.snapshot.note));
        ui.small("Stages a local setting replacement. Approval and apply are separate. No external files or network effects.");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.note);
            if ui.button("Stage note change").clicked() {
                self.command(Command::StageNote(self.note.clone()));
            }
        });
        let mut commands = vec![];
        for proposal in &self.snapshot.proposals {
            ui.group(|ui| {
                ui.label(format!(
                    "{:?} · {}",
                    proposal.status,
                    proposal.envelope["note"].as_str().unwrap_or("")
                ));
                ui.small(format!("Proposal {}", proposal.id));
                ui.horizontal(|ui| {
                    if proposal.status == ProposalStatus::Pending {
                        if ui.button("Approve").clicked() {
                            commands.push(Command::Decide {
                                proposal_id: proposal.id.clone(),
                                approve: true,
                            });
                        }
                        if ui.button("Reject").clicked() {
                            commands.push(Command::Decide {
                                proposal_id: proposal.id.clone(),
                                approve: false,
                            });
                        }
                    }
                    if matches!(
                        proposal.status,
                        ProposalStatus::Approved | ProposalStatus::Applying
                    ) && ui.button("Apply local note").clicked()
                    {
                        commands.push(Command::Apply(proposal.id.clone()));
                    }
                });
            });
        }
        for command in commands {
            self.command(command);
        }
    }

    fn verification(&self, ui: &mut egui::Ui) {
        ui.heading("Local verification");
        ui.small("Checks durable answers and local demo note receipts. It does not establish factual correctness of model output.");
        if self.snapshot.verifications.is_empty() {
            ui.label("No verification report yet.");
        }
        for report in &self.snapshot.verifications {
            ui.group(|ui| {
                ui.label(format!(
                    "{} · {}",
                    report.task_id,
                    report.data["status"].as_str().unwrap_or("unavailable")
                ));
                for deficiency in report.data["deficiencies"].as_array().into_iter().flatten() {
                    ui.label(
                        deficiency
                            .as_str()
                            .unwrap_or("Unrecognized verification detail"),
                    );
                }
            });
        }
    }

    fn consent(&mut self, context: &egui::Context) {
        let Some(prompt) = self.prompts.front() else {
            return;
        };
        let mut decision = None;
        egui::Window::new("Approve this exact request").collapsible(false).resizable(false).show(context, |ui| {
            if prompt.local_demo { ui.label("In-process demo. No network request will be made. The destination identifies the consent scope only."); }
            else { ui.label("This sends the disclosed data to the named destination using this exact connection and model."); }
            ui.monospace(format!("App: {}\nConnection: {}\nGeneration: {}\nAccount: {}\nDestination: {}\nModel: {}\nData: {}",
                prompt.app, prompt.connection, prompt.generation, prompt.account.as_deref().unwrap_or("none"),
                prompt.destination, prompt.model, prompt.categories.join(", ")));
            ui.horizontal(|ui| {
                if ui.button("Approve once").clicked() { decision = Some(true); }
                if ui.button("Deny").clicked() { decision = Some(false); }
            });
        });
        if let Some(approved) = decision
            && let Some(prompt) = self.prompts.pop_front()
        {
            let _ = prompt.reply.send(approved);
        }
    }
}

impl eframe::App for NativeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive();
        egui::CentralPanel::default().show(ui, |ui| {
            self.header(ui);
            ui.separator();
            egui::ScrollArea::vertical().id_salt("body").show(ui, |ui| {
                self.chat(ui);
                ui.separator();
                self.tasks(ui);
                ui.separator();
                self.proposals(ui);
                ui.separator();
                self.verification(ui);
            });
        });
        self.consent(ui.ctx());
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

impl Drop for NativeApp {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
