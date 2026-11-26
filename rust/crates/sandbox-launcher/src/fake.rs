//! In-memory [`RuntimeApi`] for the service and server tests, with knobs
//! for the failures the launcher must survive.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::Value;

use crate::runtime::{
    ContainerSummary, CreatedContainer, Removal, RuntimeApi, RuntimeError, VolumeInfo,
};

#[derive(Debug, Clone)]
pub(crate) struct FakeContainer {
    pub id: String,
    pub name: String,
    pub state: String,
    pub labels: BTreeMap<String, String>,
    pub body: Value,
}

#[derive(Debug, Default)]
pub(crate) struct FakeState {
    pub containers: Vec<FakeContainer>,
    pub volumes: BTreeMap<String, VolumeInfo>,
    pub sizes: BTreeMap<String, Option<u64>>,
    pub calls: Vec<String>,
    pub next_id: u64,
    /// `remove_container` leaves the container listed.
    pub sticky: bool,
    /// `create_container` answers these warnings.
    pub create_warnings: Vec<String>,
    /// `start_container` fails.
    pub fail_start: bool,
    /// Every call fails as unreachable.
    pub down: bool,
    /// `list_containers` ignores its label filter.
    pub ignore_filter: bool,
}

#[derive(Debug, Default)]
pub(crate) struct FakeRuntime {
    pub state: Mutex<FakeState>,
}

impl FakeRuntime {
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.with(|s| s.calls.clone())
    }

    pub(crate) fn count(&self, prefix: &str) -> usize {
        self.with(|s| s.calls.iter().filter(|c| c.starts_with(prefix)).count())
    }

    /// Put a container in place as if someone else created it.
    pub(crate) fn insert(&self, name: &str, state: &str, labels: &[(&str, &str)]) -> String {
        self.with(|s| {
            s.next_id += 1;
            let id = format!("{:064x}", s.next_id);
            s.containers.push(FakeContainer {
                id: id.clone(),
                name: name.to_string(),
                state: state.to_string(),
                labels: labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: Value::Null,
            });
            id
        })
    }

    pub(crate) fn set_state(&self, id: &str, state: &str) {
        self.with(|s| {
            for c in s.containers.iter_mut().filter(|c| c.id == id) {
                c.state = state.to_string();
            }
        });
    }

    fn enter(&self, call: String) -> Result<std::sync::MutexGuard<'_, FakeState>, RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(call);
        if state.down {
            return Err(RuntimeError::Unreachable("fake runtime is down".into()));
        }
        Ok(state)
    }
}

fn labels_of(body: &Value) -> BTreeMap<String, String> {
    serde_json::from_value(body["Labels"].clone()).unwrap_or_default()
}

#[async_trait]
impl RuntimeApi for FakeRuntime {
    async fn list_containers(&self, filter: &str) -> Result<Vec<ContainerSummary>, RuntimeError> {
        let s = self.enter(format!("list {filter}"))?;
        let (key, value) = filter.split_once('=').unwrap_or((filter, ""));
        Ok(s.containers
            .iter()
            .filter(|c| s.ignore_filter || c.labels.get(key).map(String::as_str) == Some(value))
            .map(|c| ContainerSummary {
                id: c.id.clone(),
                state: c.state.clone(),
                labels: c.labels.clone(),
            })
            .collect())
    }

    async fn container_exists(&self, name: &str) -> Result<bool, RuntimeError> {
        let s = self.enter(format!("exists {name}"))?;
        Ok(s.containers.iter().any(|c| c.name == name))
    }

    async fn container_env(&self, id: &str) -> Result<Vec<String>, RuntimeError> {
        let s = self.enter(format!("env {id}"))?;
        let container =
            s.containers
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| RuntimeError::Status {
                    status: 404,
                    message: "no such container".into(),
                })?;
        Ok(serde_json::from_value(container.body["Env"].clone()).unwrap_or_default())
    }

    async fn create_container(
        &self,
        name: &str,
        body: &Value,
    ) -> Result<CreatedContainer, RuntimeError> {
        let mut s = self.enter(format!("create {name}"))?;
        if s.containers.iter().any(|c| c.name == name) {
            return Err(RuntimeError::Status {
                status: 409,
                message: "name in use".into(),
            });
        }
        s.next_id += 1;
        let id = format!("{:064x}", s.next_id);
        let container = FakeContainer {
            id: id.clone(),
            name: name.to_string(),
            state: "created".into(),
            labels: labels_of(body),
            body: body.clone(),
        };
        s.containers.push(container);
        Ok(CreatedContainer {
            id,
            warnings: s.create_warnings.clone(),
        })
    }

    async fn start_container(&self, id: &str) -> Result<(), RuntimeError> {
        let mut s = self.enter(format!("start {id}"))?;
        if s.fail_start {
            return Err(RuntimeError::Status {
                status: 500,
                message: "cannot start".into(),
            });
        }
        for c in s.containers.iter_mut().filter(|c| c.id == id) {
            c.state = "running".into();
        }
        Ok(())
    }

    async fn stop_container(&self, id: &str, grace_secs: u64) -> Result<(), RuntimeError> {
        let mut s = self.enter(format!("stop {id} t={grace_secs}"))?;
        for c in s.containers.iter_mut().filter(|c| c.id == id) {
            c.state = "exited".into();
        }
        Ok(())
    }

    async fn remove_container(&self, id: &str) -> Result<(), RuntimeError> {
        let mut s = self.enter(format!("remove {id}"))?;
        if !s.sticky {
            s.containers.retain(|c| c.id != id);
        }
        Ok(())
    }

    async fn inspect_volume(&self, name: &str) -> Result<Option<VolumeInfo>, RuntimeError> {
        let s = self.enter(format!("inspect_volume {name}"))?;
        Ok(s.volumes.get(name).cloned())
    }

    async fn create_volume(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), RuntimeError> {
        let mut s = self.enter(format!("create_volume {name}"))?;
        let volume = VolumeInfo {
            driver: "local".into(),
            labels: labels.clone(),
            options: BTreeMap::new(),
        };
        s.volumes.insert(name.to_string(), volume);
        Ok(())
    }

    async fn remove_volume(&self, name: &str) -> Result<Removal, RuntimeError> {
        let mut s = self.enter(format!("remove_volume {name}"))?;
        Ok(match s.volumes.remove(name) {
            Some(_) => Removal::Removed,
            None => Removal::Absent,
        })
    }

    async fn volume_sizes(&self) -> Result<BTreeMap<String, Option<u64>>, RuntimeError> {
        let s = self.enter("df".into())?;
        Ok(s.sizes.clone())
    }
}

/// Digest of the test configurations.
pub(crate) const DIGEST: &str =
    "sha256:984736245081e2d2fecd711ead271b0c9ccf273fe096efc12a8dd23e59f9aa0a";

/// A configuration rooted at `root` (a canonical scratch directory): the
/// socket host directory and the state directory below it, the server
/// being the test process's own uid and the root's gid.
pub(crate) fn test_config(root: &std::path::Path) -> crate::config::Config {
    use std::os::unix::fs::MetadataExt;
    let gid = std::fs::metadata(root).unwrap().gid();
    let uid = agentkit_sys::effective_uid().unwrap();
    let value = serde_json::json!({
        "runtime_socket": root.join("runtime.sock"),
        "listen_socket": root.join("launcher.sock"),
        "image_reference": format!("localhost/agentkit-sandbox@{DIGEST}"),
        "image_digest": DIGEST,
        "socket_host_dir": root.join("sockets"),
        "server_uid": uid,
        "server_gid": gid,
        "state_dir": root.join("state"),
    });
    let config: crate::config::Config = serde_json::from_value(value).unwrap();
    config.validate().unwrap();
    config
}

/// A canonical scratch root (macOS temp paths run through `/var`, a link)
/// with `sockets` (0700: the launcher and the server are the test user)
/// and `state` (0755) created. Checked walks start at the root
/// ([`crate::trust::TrustRoot::at`]), which is checked too, so its mode is
/// set (0755): under a umask of 002 it would be group-writable.
pub(crate) fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (sub, mode) in [("sockets", 0o700), ("state", 0o755)] {
        std::fs::create_dir(root.join(sub)).unwrap();
        std::fs::set_permissions(root.join(sub), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    (dir, root)
}
