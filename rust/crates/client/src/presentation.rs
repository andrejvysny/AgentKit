use crate::{ClientError, RunState, sse::protocol};
use agentkit_contracts::{MessageDto, ProposalDto};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLoad {
    chat_id: String,
    revision: u64,
}

/// UI-owned state. Load tickets prevent a late response from replacing a new chat.
#[derive(Clone, Debug, Default)]
pub struct ChatState {
    chat_id: Option<String>,
    revision: u64,
    messages: Vec<MessageDto>,
    proposals: Vec<ProposalDto>,
    runs: BTreeMap<String, RunState>,
}

impl ChatState {
    pub fn select(&mut self, chat_id: &str) -> ChatLoad {
        self.revision = self.revision.checked_add(1).expect("UI revision exhausted");
        self.chat_id = Some(chat_id.into());
        self.messages.clear();
        self.proposals.clear();
        self.runs.clear();
        ChatLoad {
            chat_id: chat_id.into(),
            revision: self.revision,
        }
    }

    pub fn replace(
        &mut self,
        load: &ChatLoad,
        messages: Vec<MessageDto>,
        proposals: Vec<ProposalDto>,
    ) -> Result<bool, ClientError> {
        if !self.current(load) {
            return Ok(false);
        }
        if messages
            .iter()
            .any(|message| message.chat_id != load.chat_id)
            || proposals
                .iter()
                .any(|proposal| proposal.chat_id != load.chat_id)
        {
            return Err(protocol("Snapshot contains another chat"));
        }
        self.messages = messages;
        self.proposals = proposals;
        Ok(true)
    }

    pub fn observe(&mut self, load: &ChatLoad, event: &Value) -> Result<bool, ClientError> {
        if !self.current(load) {
            return Ok(false);
        }
        let run = event["runId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| protocol("Event runId is required"))?;
        self.runs.entry(run.into()).or_default().observe(event)
    }

    pub fn settle(&mut self, load: &ChatLoad, run: &str, status: &str) -> bool {
        if !self.current(load) {
            return false;
        }
        self.runs.entry(run.into()).or_default().status = Some(status.into());
        true
    }

    pub fn messages(&self) -> &[MessageDto] {
        &self.messages
    }
    pub fn proposals(&self) -> &[ProposalDto] {
        &self.proposals
    }
    pub fn runs(&self) -> &BTreeMap<String, RunState> {
        &self.runs
    }

    fn current(&self, load: &ChatLoad) -> bool {
        self.chat_id.as_deref() == Some(&load.chat_id) && self.revision == load.revision
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionChoice {
    pub connection_id: String,
    pub generation: u64,
    pub model: String,
}

/// Selection is explicit. Refresh invalidates a missing choice without selecting a fallback.
#[derive(Clone, Debug, Default)]
pub struct ConnectionState {
    choices: Vec<ConnectionChoice>,
    selected: Option<ConnectionChoice>,
}

impl ConnectionState {
    pub fn refresh(&mut self, choices: Vec<ConnectionChoice>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|choice| !choices.contains(choice))
        {
            self.selected = None;
        }
        self.choices = choices;
    }

    pub fn select(&mut self, choice: &ConnectionChoice) -> Result<(), ClientError> {
        if !self.choices.contains(choice) {
            return Err(protocol(
                "Connection/model selection is stale or unavailable",
            ));
        }
        self.selected = Some(choice.clone());
        Ok(())
    }

    pub fn selected(&self) -> Option<&ConnectionChoice> {
        self.selected.as_ref()
    }
    pub fn choices(&self) -> &[ConnectionChoice] {
        &self.choices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunPhase;
    use serde_json::json;

    #[test]
    fn stale_chat_events_and_snapshots_cannot_replace_current_selection() {
        let mut state = ChatState::default();
        let old = state.select("old");
        let current = state.select("new");
        let event =
            json!({"runId":"run","seq":0,"eventId":"event","type":"run.completed","data":{}});
        assert!(!state.observe(&old, &event).unwrap());
        assert!(!state.replace(&old, vec![], vec![]).unwrap());
        assert!(state.observe(&current, &event).unwrap());
        assert_eq!(state.runs()["run"].phase(), RunPhase::Settling);
        state.settle(&current, "run", "completed");
        assert_eq!(state.runs()["run"].phase(), RunPhase::Completed);
    }

    #[test]
    fn generation_change_invalidates_selection_without_switching_accounts() {
        let first = ConnectionChoice {
            connection_id: "account-a".into(),
            generation: 1,
            model: "model".into(),
        };
        let changed = ConnectionChoice {
            generation: 2,
            ..first.clone()
        };
        let mut state = ConnectionState::default();
        state.refresh(vec![first.clone()]);
        state.select(&first).unwrap();
        state.refresh(vec![changed.clone()]);
        assert!(state.selected().is_none());
        assert!(state.select(&first).is_err());
        state.select(&changed).unwrap();
        assert_eq!(state.selected(), Some(&changed));
    }
}
