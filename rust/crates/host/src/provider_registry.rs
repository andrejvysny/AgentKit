use agentkit_core::ChatProvider;
use std::{collections::BTreeMap, sync::Arc};

/// Immutable providerId -> provider map. Built once before the executor
/// starts and never mutated, so a durable task payload's `providerId`
/// resolves to the same provider for the whole service lifetime.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn ChatProvider>>,
}

impl ProviderRegistry {
    /// Keys are each provider's own `id()`; a later duplicate id replaces an
    /// earlier one, so callers validate uniqueness before building.
    pub fn new(providers: impl IntoIterator<Item = Arc<dyn ChatProvider>>) -> Self {
        Self {
            providers: providers
                .into_iter()
                .map(|provider| (provider.id().to_owned(), provider))
                .collect(),
        }
    }

    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn ChatProvider>> {
        self.providers.get(provider_id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}
