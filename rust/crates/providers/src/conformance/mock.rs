//! Deterministic mock model. It follows the task's scripted turns, or the turns
//! derived from `expect.calls`, so no network or provider is involved.
use super::runner::{Model, ModelCall, ModelTurn, RunCtx};
use super::task::{ExpectedCall, Task};
use async_trait::async_trait;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockMode {
    /// Issues the expected calls with the expected arguments.
    Perfect,
    /// Every call names a tool that does not exist (counts as invalid).
    Broken,
    /// Well-formed calls with wrong values: few invalid calls, tasks fail.
    WrongArgs,
    /// `Broken` on every n-th task run (by matrix sequence), `Perfect` otherwise.
    FailEvery(usize),
}

pub struct MockModel {
    name: String,
    mode: MockMode,
}
impl MockModel {
    pub fn new(name: &str, mode: MockMode) -> Self {
        Self {
            name: name.to_owned(),
            mode,
        }
    }

    fn effective(&self, ctx: &RunCtx) -> MockMode {
        match self.mode {
            MockMode::FailEvery(n) if n > 0 && ctx.seq % n == n - 1 => MockMode::Broken,
            MockMode::FailEvery(_) => MockMode::Perfect,
            other => other,
        }
    }
}

fn concrete(call: &ExpectedCall, mode: MockMode, id: String) -> ModelCall {
    let args: Map<String, Value> = call
        .args
        .iter()
        .map(|(key, value)| {
            let text = match value.as_str() {
                Some("*") => "x".to_owned(),
                Some(s) if s.starts_with("contains:") => s["contains:".len()..].to_owned(),
                Some(s) => s.to_owned(),
                None => value.to_string(),
            };
            let text = if mode == MockMode::WrongArgs {
                format!("{text}_x")
            } else {
                text
            };
            (key.clone(), Value::String(text))
        })
        .collect();
    let name = if mode == MockMode::Broken {
        format!("{}_v2", call.tool)
    } else {
        call.tool.clone()
    };
    ModelCall {
        id,
        name,
        arguments: Value::Object(args).to_string(),
    }
}

#[async_trait]
impl Model for MockModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        task: &Task,
        ctx: &RunCtx,
        _messages: &[Value],
    ) -> Result<ModelTurn, String> {
        let mode = self.effective(ctx);
        let derived: Vec<Vec<ExpectedCall>> = if task.expect.calls.is_empty() {
            Vec::new()
        } else {
            vec![task.expect.calls.clone()]
        };
        let (turns, final_text): (&[Vec<ExpectedCall>], String) = match &task.mock {
            Some(script) => (&script.turns[..], script.final_text.clone()),
            None => (
                &derived[..],
                if task.expect.answer_contains.is_empty() {
                    "Done.".to_owned()
                } else {
                    task.expect.answer_contains.join(" ")
                },
            ),
        };
        match turns.get(ctx.turn as usize) {
            Some(calls) => Ok(ModelTurn {
                text: String::new(),
                calls: calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| concrete(c, mode, format!("call_{}_{i}", ctx.turn)))
                    .collect(),
            }),
            None => Ok(ModelTurn {
                text: final_text,
                calls: Vec::new(),
            }),
        }
    }
}
