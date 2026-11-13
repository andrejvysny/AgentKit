use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Frozen AgentKit defaults. A preset never authorizes probing or account selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPreset {
    pub kind: String,
    pub label: String,
    pub default_base_url: String,
    pub default_model: String,
    pub requires_api_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_base_urls: Option<Vec<String>>,
}

pub fn provider_presets() -> &'static [ProviderPreset] {
    static PRESETS: OnceLock<Vec<ProviderPreset>> = OnceLock::new();
    PRESETS
        .get_or_init(|| serde_json::from_str(include_str!("presets.json")).expect("frozen presets"))
}

pub fn preset_by_kind(kind: &str) -> Option<&'static ProviderPreset> {
    provider_presets().iter().find(|preset| preset.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_preserve_frozen_defaults_and_open_kind_vocabulary() {
        assert_eq!(provider_presets().len(), 6);
        assert_eq!(preset_by_kind("omlx").unwrap().default_base_url, "");
        assert_eq!(preset_by_kind("ollama").unwrap().default_model, "llama3.2");
        assert!(preset_by_kind("custom-host-adapter").is_none());
        let frozen: serde_json::Value = serde_json::from_str(include_str!("presets.json")).unwrap();
        assert_eq!(serde_json::to_value(provider_presets()).unwrap(), frozen);
    }
}
