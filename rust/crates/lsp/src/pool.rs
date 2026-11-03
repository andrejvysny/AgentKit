//! Pool types. (LspPool itself lands below the lifecycle manager.)

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use agentkit_core::CancellationToken;
use serde::{Deserialize, Serialize};

use crate::error::{INVALID_FILE, INVALID_INPUT, err};
use crate::server::ServerHandle;

/// One launchable language server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Tunables for the whole pool. Defaults per the product spec.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct LspConfig {
    /// Explicit per-language command overrides (`rust`, `typescript`).
    pub servers: BTreeMap<String, ServerCommand>,
    pub request_timeout_ms: u64,
    pub init_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub diagnostics_wait_ms: u64,
    pub max_restarts: usize,
    pub restart_window_ms: u64,
    /// Opt-in ONLY: running a workspace-local `node_modules/.bin` server
    /// means executing a binary planted by whoever wrote that repository —
    /// code execution on the host the moment a model first calls an
    /// `lsp_*` tool. PATH / `AGENTKIT_LSP_*` / explicit `servers` entries
    /// are operator decisions; a checked-out repository is not.
    #[serde(default)]
    pub allow_workspace_binaries: bool,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            servers: BTreeMap::new(),
            request_timeout_ms: 30_000,
            init_timeout_ms: 60_000,
            idle_timeout_ms: 300_000,
            diagnostics_wait_ms: 3_000,
            max_restarts: 3,
            restart_window_ms: 60_000,
            allow_workspace_binaries: false,
        }
    }
}

impl LspConfig {
    /// Parse the JSON binding form (`{"requestTimeoutMs": 1000,
    /// "servers": {"rust": {"command": "rust-analyzer"}}}`).
    pub fn from_json(json: &str) -> Result<Self, agentkit_core::CoreError> {
        let mut config: Self = serde_json::from_str(json).map_err(|e| {
            err(
                INVALID_INPUT,
                format!("invalid LSP configuration JSON: {e}"),
            )
        })?;
        config.normalize();
        Ok(config)
    }

    /// Clamp the JS-supplied millisecond bounds into `[1 ms, 1 h]`: a huge
    /// value feeds `tokio::time::sleep`/`timeout` and `Instant + duration`
    /// and PANICS past tokio's timer ceiling / on `Instant` overflow, while
    /// `0` silently disables the guard (every request fails instantly, or
    /// the server is evicted and respawned on every acquire). `max_restarts`
    /// stays finite so the crash budget cannot become a respawn loop.
    pub fn normalize(&mut self) {
        const MAX_MS: u64 = 3_600_000;
        for field in [
            &mut self.request_timeout_ms,
            &mut self.init_timeout_ms,
            &mut self.idle_timeout_ms,
            &mut self.diagnostics_wait_ms,
            &mut self.restart_window_ms,
        ] {
            *field = (*field).clamp(1, MAX_MS);
        }
        self.max_restarts = self.max_restarts.min(100);
    }

    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.request_timeout_ms)
    }
    pub(crate) fn init_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.init_timeout_ms)
    }
    pub(crate) fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.idle_timeout_ms)
    }
    pub(crate) fn diagnostics_wait(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.diagnostics_wait_ms)
    }
    pub(crate) fn restart_window(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.restart_window_ms)
    }
}

/// Shared pool of language servers, keyed by (canonical workspace root,
/// language). Two agents hitting the same workspace reuse ONE server
/// process; the slot's init lock prevents double spawn.
pub struct LspPool {
    config: Arc<LspConfig>,
    slots: std::sync::Mutex<HashMap<(PathBuf, String), Arc<ServerHandle>>>,
}

impl LspPool {
    pub fn new(mut config: LspConfig) -> Arc<Self> {
        // Host-constructed configs get the same boundary clamp as the
        // JSON form — a hostile or mistuned `optionsJson` must never reach
        // a timer.
        config.normalize();
        Arc::new(Self {
            config: Arc::new(config),
            slots: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    /// Canonical root for tool-facing paths (`lsp_invalid_file` when the
    /// workspace is not a usable directory).
    pub fn canonical_root(root: &Path) -> Result<PathBuf, agentkit_core::CoreError> {
        let canonical = root.canonicalize().map_err(|e| {
            err(
                INVALID_FILE,
                format!("workspace root {} is unusable: {e}", root.display()),
            )
        })?;
        if !canonical.is_dir() {
            return Err(err(
                INVALID_FILE,
                format!("workspace root {} is not a directory", root.display()),
            ));
        }
        Ok(canonical)
    }

    /// Get (spawn-on-demand) the healthy server handle for this workspace
    /// and language. Evicts idle servers on the way in.
    pub async fn acquire(
        self: &Arc<Self>,
        root: &Path,
        language: &str,
        cancellation: &CancellationToken,
    ) -> Result<Arc<ServerHandle>, agentkit_core::CoreError> {
        let canonical = Self::canonical_root(root)?;
        self.evict_idle();
        let key = (canonical.clone(), language.to_owned());
        let handle = {
            let mut slots = self.slots.lock().expect("slots lock");
            if let Some(handle) = slots.get(&key) {
                let handle = Arc::clone(handle);
                drop(slots);
                handle
            } else {
                let command = crate::discovery::resolve_server_command(
                    &self.config.servers,
                    language,
                    &canonical,
                    self.config.allow_workspace_binaries,
                )?;
                let handle = Arc::new(ServerHandle::new(
                    canonical.clone(),
                    language.to_owned(),
                    command,
                    Arc::clone(&self.config),
                ));
                slots.insert(key, Arc::clone(&handle));
                drop(slots);
                handle
            }
        };
        // Force a live process now so spawn failures surface at acquire.
        handle.acquire(cancellation).await?;
        Ok(handle)
    }

    /// Observability for pool-sharing assertions: `(generation, pid)` of
    /// the live server, `None` when nothing is running.
    pub fn slot_status(&self, root: &Path, language: &str) -> Option<(u64, u32)> {
        let canonical = root.canonicalize().ok()?;
        let slots = self.slots.lock().expect("slots lock");
        let handle = slots.get(&(canonical, language.to_owned()))?;
        let generation = handle.live_generation()?;
        let pid = handle.live_pid()?;
        Some((generation, pid))
    }

    /// Remove + gracefully shut down (in the background) every server
    /// unused past `idle_timeout_ms`.
    pub fn evict_idle(&self) {
        let expired: Vec<Arc<ServerHandle>> = {
            let mut slots = self.slots.lock().expect("slots lock");
            let expired: Vec<(PathBuf, String)> = slots
                .iter()
                .filter(|(_, handle)| handle.is_idle(self.config.idle_timeout()))
                .map(|((root, language), _)| (root.clone(), language.clone()))
                .collect();
            expired.iter().filter_map(|key| slots.remove(key)).collect()
        };
        for handle in expired {
            tokio::spawn(async move { handle.close().await });
        }
    }

    /// Shut down every running server (bounded by the per-server grace).
    pub async fn close(&self) {
        let handles: Vec<Arc<ServerHandle>> = self
            .slots
            .lock()
            .expect("slots lock")
            .drain()
            .map(|(_, h)| h)
            .collect();
        let mut set = tokio::task::JoinSet::new();
        for handle in handles {
            set.spawn(async move { handle.close().await });
        }
        let _ = tokio::time::timeout(SHUTDOWN_SWEEP, set.join_all()).await;
    }
}

const SHUTDOWN_SWEEP: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_config_bounds_are_clamped_into_timer_safe_ranges() {
        // A hostile or mistuned `optionsJson` must never reach a tokio
        // timer: past the timer ceiling `sleep`/`timeout` PANIC the host,
        // and `0` silently disables the guard (requests die instantly or
        // the server is evicted and respawned on every acquire).
        let huge = LspConfig::from_json(
            r#"{"requestTimeoutMs":18446744073709551615,"initTimeoutMs":99999999999999,
               "idleTimeoutMs":0,"diagnosticsWaitMs":0,"restartWindowMs":0,
               "maxRestarts":1000000}"#,
        )
        .expect("valid JSON binds");
        assert_eq!(huge.request_timeout_ms, 3_600_000);
        assert_eq!(huge.init_timeout_ms, 3_600_000);
        assert_eq!(huge.idle_timeout_ms, 1);
        assert_eq!(huge.diagnostics_wait_ms, 1);
        assert_eq!(huge.restart_window_ms, 1);
        assert_eq!(huge.max_restarts, 100);

        let mut host_side = LspConfig {
            request_timeout_ms: u64::MAX,
            ..Default::default()
        };
        host_side.normalize();
        assert_eq!(host_side.request_timeout_ms, 3_600_000);

        // The pool constructor normalizes host-constructed configs too,
        // so no construction route reaches a timer with an untuned value.
        let pool = LspPool::new(LspConfig {
            idle_timeout_ms: u64::MAX,
            diagnostics_wait_ms: 0,
            ..Default::default()
        });
        assert_eq!(pool.config().idle_timeout_ms, 3_600_000);
        assert_eq!(pool.config().diagnostics_wait_ms, 1);
    }

    #[test]
    fn defaults_survive_normalization_untouched() {
        let mut expected = LspConfig::default();
        expected.normalize();
        let config = LspConfig::from_json("{}").expect("empty object binds to defaults");
        assert_eq!(config, expected);
    }
}
