use crate::{HostError, HostResult};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteRisk {
    Low,
    Medium,
    High,
    Destructive,
}

impl WriteRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicyMode {
    AutoReadonlyConfirmWrites,
    ConfirmAllWrites,
    AutoAll,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteQuery {
    pub chat_id: String,
    pub tool_name: String,
    pub proposal_kind: String,
    pub scope_key: String,
    pub risk: WriteRisk,
    pub actor_id: Option<String>,
    pub payload_fingerprint: String,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteAllowance {
    pub chat_id: String,
    pub tool_name: String,
    pub proposal_kind: String,
    pub scope_key: Option<String>,
    pub max_risk: WriteRisk,
    pub actor_id: Option<String>,
    pub payload_fingerprint: Option<String>,
    pub revision: Option<String>,
}

/// Policy answers are synchronous; host state can revoke a grant between calls.
pub trait ToolWritePolicy: Send + Sync {
    fn is_auto_apply_allowed(&self, query: &WriteQuery) -> bool;
}

struct State {
    mode: WritePolicyMode,
    allowances: BTreeMap<String, WriteAllowance>,
}

/// Grants are deliberately ephemeral. Reopening a process does not restore consent.
pub struct SessionWritePolicy {
    state: Mutex<State>,
}

impl Default for SessionWritePolicy {
    fn default() -> Self {
        Self::new(WritePolicyMode::AutoReadonlyConfirmWrites)
    }
}

impl SessionWritePolicy {
    pub fn new(mode: WritePolicyMode) -> Self {
        Self {
            state: Mutex::new(State {
                mode,
                allowances: BTreeMap::new(),
            }),
        }
    }

    pub fn mode(&self) -> HostResult<WritePolicyMode> {
        Ok(self.state.lock().map_err(|_| poisoned())?.mode)
    }

    pub fn set_mode(&self, mode: WritePolicyMode) -> HostResult<()> {
        self.state.lock().map_err(|_| poisoned())?.mode = mode;
        Ok(())
    }

    pub fn allow(&self, allowance: WriteAllowance) -> HostResult<String> {
        if allowance.actor_id.is_some()
            && (allowance.scope_key.is_none()
                || allowance
                    .payload_fingerprint
                    .as_deref()
                    .is_none_or(str::is_empty))
        {
            return Err(HostError::Execution(
                "actor_allowance_requires_exact_scope_and_payload".into(),
            ));
        }
        let key = allowance_key(&allowance);
        self.state
            .lock()
            .map_err(|_| poisoned())?
            .allowances
            .insert(key.clone(), allowance);
        Ok(key)
    }

    pub fn list(&self, chat_id: &str) -> HostResult<Vec<(String, WriteAllowance)>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| poisoned())?
            .allowances
            .iter()
            .filter(|(_, grant)| grant.chat_id == chat_id)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }

    pub fn revoke(&self, chat_id: &str, key: &str) -> HostResult<()> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        if state
            .allowances
            .get(key)
            .is_some_and(|grant| grant.chat_id == chat_id)
        {
            state.allowances.remove(key);
        }
        Ok(())
    }

    pub fn revoke_actor(&self, actor_id: &str) -> HostResult<()> {
        self.state
            .lock()
            .map_err(|_| poisoned())?
            .allowances
            .retain(|_, grant| grant.actor_id.as_deref() != Some(actor_id));
        Ok(())
    }
}

impl ToolWritePolicy for SessionWritePolicy {
    fn is_auto_apply_allowed(&self, query: &WriteQuery) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if state.mode == WritePolicyMode::ConfirmAllWrites {
            return false;
        }
        if query.actor_id.is_none() && state.mode == WritePolicyMode::AutoAll {
            return true;
        }
        state
            .allowances
            .values()
            .any(|grant| matches_grant(grant, query))
    }
}

fn matches_grant(grant: &WriteAllowance, query: &WriteQuery) -> bool {
    grant.chat_id == query.chat_id
        && grant.tool_name == query.tool_name
        && grant.proposal_kind == query.proposal_kind
        && grant.max_risk >= query.risk
        && grant
            .scope_key
            .as_ref()
            .is_none_or(|scope| *scope == query.scope_key)
        && grant.actor_id == query.actor_id
        && (query.actor_id.is_none()
            || (grant.scope_key.as_ref() == Some(&query.scope_key)
                && grant.payload_fingerprint.as_ref() == Some(&query.payload_fingerprint)
                && grant.revision == query.revision))
}

fn allowance_key(grant: &WriteAllowance) -> String {
    if grant.actor_id.is_none() {
        return serde_json::json!([
            grant.chat_id,
            grant.tool_name,
            grant.proposal_kind,
            grant.scope_key
        ])
        .to_string();
    }
    serde_json::json!([
        grant.chat_id,
        grant.tool_name,
        grant.proposal_kind,
        grant.scope_key,
        grant.actor_id,
        grant.payload_fingerprint,
        grant.revision
    ])
    .to_string()
}

fn poisoned() -> HostError {
    HostError::Execution("write_policy_unavailable".into())
}
