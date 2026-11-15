//! Carrying registry-level metadata onto a registry that holds a subset of
//! another's tools (a profile's tool filter). Tools bring their own argument
//! aliases and toolset declarations when they are registered; everything the
//! host registered on the registry itself (toolsets, retired names, global
//! aliases, withheld reasons) is carried here, limited to what still applies.

use super::ToolRegistry;

impl ToolRegistry {
    /// Copy `source`'s toolsets, retired names, aliases and withheld reasons
    /// onto this registry, which already holds the tools that remain.
    ///
    /// - A toolset keeps only its remaining members and is dropped when none
    ///   remain (nothing in it could be loaded).
    /// - A retired name keeps answering when it teaches (no replacement) or
    ///   its replacement remains; one whose replacement was filtered out is
    ///   dropped, so the call reads as unknown like any excluded tool.
    /// - A withheld reason is kept for a tool that remains or that `keep`
    ///   still names.
    pub fn inherit_metadata(&mut self, source: &ToolRegistry, keep: &dyn Fn(&str) -> bool) {
        self.aliases = source.aliases.clone();
        let remains = |registry: &ToolRegistry, name: &str| {
            registry.tools.contains_key(name) || registry.ghosts.contains_key(name)
        };
        let sets: Vec<_> = source
            .toolsets
            .values()
            .filter_map(|set| {
                let members: Vec<String> = set
                    .tools
                    .iter()
                    .filter(|name| remains(self, name))
                    .cloned()
                    .collect();
                let declared = self.declared.values().any(|id| id == &set.id);
                (!members.is_empty() || declared).then(|| {
                    let mut kept = set.clone();
                    kept.tools = members;
                    kept
                })
            })
            .collect();
        for set in sets {
            self.register_toolset(set);
        }
        for legacy in source.legacy.values() {
            let target_remains = legacy
                .new
                .as_deref()
                .is_none_or(|new| self.tools.contains_key(new));
            if target_remains {
                self.register_legacy_name(legacy.clone());
            }
        }
        for (name, reason) in &source.withheld {
            if (remains(self, name) || keep(name)) && !self.withheld.contains_key(name) {
                self.withheld.insert(name.clone(), reason.clone());
            }
        }
    }
}
