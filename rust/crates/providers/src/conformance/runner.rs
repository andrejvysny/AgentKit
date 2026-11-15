//! Task runner: drives a [`Model`] against the fake workspace and checks the result.
use super::check::{CallRecord, Sandbox, check_task};
use super::matrix::{Matrix, Thresholds, build_matrix};
use super::task::Task;
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCall {
    pub id: String,
    pub name: String,
    /// Raw argument text, exactly as the model produced it.
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelTurn {
    pub text: String,
    pub calls: Vec<ModelCall>,
}

/// Position of a turn inside a matrix run.
#[derive(Clone, Copy, Debug)]
pub struct RunCtx {
    /// Index of this task run across the whole matrix pass (task-major).
    pub seq: usize,
    /// Zero-based repeat of the task.
    pub run: u32,
    /// Zero-based model turn within the run.
    pub turn: u32,
}

#[async_trait]
pub trait Model: Send + Sync {
    fn name(&self) -> &str;
    /// `messages` is the OpenAI-style history so far (system, user, assistant, tool).
    async fn turn(
        &self,
        task: &Task,
        ctx: &RunCtx,
        messages: &[Value],
    ) -> Result<ModelTurn, String>;
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub task_id: String,
    pub toolset: String,
    pub run: u32,
    pub success: bool,
    pub turns: u32,
    pub calls: Vec<CallRecord>,
    /// Why the run failed; `None` on success.
    pub failure: Option<String>,
}

const SYSTEM_PROMPT: &str =
    "You are a careful assistant. Use the provided tools to complete the task, then answer briefly.";
const ANNOUNCE: [&str; 8] = [
    "let me",
    "i will",
    "i'll",
    "i am going to",
    "zavolám",
    "použijem",
    "pozriem",
    "idem ",
];

fn announces(text: &str) -> bool {
    let lower = text.to_lowercase();
    ANNOUNCE.iter().any(|phrase| lower.contains(phrase))
}

pub async fn run_task(model: &dyn Model, task: &Task, run: u32, seq: usize) -> RunOutcome {
    let mut sandbox = Sandbox::new(task);
    let mut messages = vec![
        json!({"role":"system","content":SYSTEM_PROMPT}),
        json!({"role":"user","content":task.prompt}),
    ];
    let (mut success, mut failure, mut turns) = (false, None, 0);
    for turn in 0..task.max_turns {
        turns = turn + 1;
        let ctx = RunCtx { seq, run, turn };
        let reply = match model.turn(task, &ctx, &messages).await {
            Ok(reply) => reply,
            Err(error) => {
                failure = Some(format!("model error: {error}"));
                break;
            }
        };
        let tool_calls: Vec<Value> = reply
            .calls
            .iter()
            .map(|c| json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}}))
            .collect();
        let mut assistant = json!({"role":"assistant","content":reply.text});
        if !tool_calls.is_empty() {
            assistant["tool_calls"] = Value::Array(tool_calls);
        }
        messages.push(assistant);
        if reply.calls.is_empty() {
            match check_task(task, &sandbox, &reply.text) {
                Ok(()) => success = true,
                Err(reason) => {
                    if announces(&reply.text) {
                        sandbox.record_announced(turns);
                    }
                    failure = Some(reason);
                }
            }
            break;
        }
        for call in &reply.calls {
            let result = sandbox.call(turns, &call.name, &call.arguments);
            messages.push(json!({"role":"tool","tool_call_id":call.id,"content":result}));
        }
        if turn + 1 == task.max_turns {
            failure = Some(format!("not finished within {} turns", task.max_turns));
        }
    }
    RunOutcome {
        task_id: task.id.clone(),
        toolset: task.toolset.clone(),
        run,
        success,
        turns,
        calls: sandbox.records,
        failure: if success { None } else { failure },
    }
}

/// Runs every task `runs` times for each model and builds the matrix.
pub async fn run_matrix(
    models: &[&dyn Model],
    tasks: &[Task],
    runs: u32,
    thresholds: Thresholds,
) -> Matrix {
    let mut outcomes = Vec::new();
    for model in models {
        let mut model_outcomes = Vec::new();
        for (index, task) in tasks.iter().enumerate() {
            for run in 0..runs {
                let seq = index * runs as usize + run as usize;
                model_outcomes.push(run_task(*model, task, run, seq).await);
            }
        }
        outcomes.push((model.name().to_owned(), model_outcomes));
    }
    build_matrix(outcomes, runs, thresholds)
}
