//! Chat Completions servers that refuse function tools together with a
//! reasoning effort (hardening H1.5b). OpenAI's newer reasoning models answer
//! such a request with a 400 whose `param` is `reasoning_effort` and ask for
//! `reasoning_effort: "none"`; leaving the field out does not help, because
//! the model's default effort counts. The provider retries once with "none"
//! and sends "none" for that model from then on.
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex, PoisonError},
};

pub(crate) const WARNING_CODE: &str = "reasoning_effort_dropped";

pub(crate) fn has_tools(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
}

/// A provider error whose `param` names `reasoning_effort`.
pub(crate) fn blames_effort(detail: &str) -> bool {
    serde_json::from_str::<Value>(detail)
        .is_ok_and(|error| error["error"]["param"] == "reasoning_effort")
}

/// Sets `reasoning_effort` to "none"; false when it already was.
pub(crate) fn force_none(body: &mut Value) -> bool {
    if body["reasoning_effort"] == "none" {
        return false;
    }
    body["reasoning_effort"] = "none".into();
    true
}

pub(crate) fn warning_message(model: &str) -> String {
    format!(
        "Model {model} refuses function tools with a reasoning effort on /chat/completions; reasoning_effort was sent as \"none\"."
    )
}

/// Models of one provider instance whose requests with tools were refused
/// for their effort. Owned by the provider (no process-global state); clones
/// of the provider share it.
#[derive(Clone, Default)]
pub(crate) struct Refusals(Arc<Mutex<HashSet<String>>>);

impl Refusals {
    pub(crate) fn contains(&self, model: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(model)
    }

    pub(crate) fn remember(&self, model: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(model.to_owned());
    }
}
