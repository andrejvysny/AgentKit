//! Which sessions may use a workspace-scoped lane (coding, LSP).
//!
//! A lane is registered for ONE workspace root. A session whose descriptor
//! names a different workspace must never receive the lane's tools or its
//! standing approvals, and because subagent children copy the parent's
//! descriptor workspace at spawn, binding by descriptor covers them too.

use std::path::{Path, PathBuf};

use agentkit_store::SqliteStore;

/// How a session relates to the lane's registered root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionBinding {
    /// The descriptor workspace is the lane root.
    Bound,
    /// No descriptor workspace to compare (no chat, no session descriptor).
    Unbound,
    /// A different workspace, an unreadable one, or a failed lookup.
    Mismatch,
}

/// Canonicalize a lane root once at registration. `None` (unresolvable)
/// makes every session that names a workspace a `Mismatch`.
pub(crate) fn canonical_lane_root(root: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(root).ok()
}

/// Classify a session against `lane_root` (already canonical). Fails
/// closed: a store error is a `Mismatch`, never an `Unbound` pass.
pub(crate) async fn session_binding(
    store: &SqliteStore,
    chat_id: Option<&str>,
    lane_root: Option<&Path>,
) -> SessionBinding {
    let Some(chat_id) = chat_id else {
        return SessionBinding::Unbound;
    };
    let chat = match store.get_chat(chat_id).await {
        Ok(Some(chat)) => chat,
        Ok(None) => return SessionBinding::Unbound,
        Err(_) => return SessionBinding::Mismatch,
    };
    let Some(root) = agentkit_host::read_session_descriptor(&chat.metadata)
        .and_then(|descriptor| descriptor.get("workspace").cloned())
        .and_then(|workspace| workspace.get("root")?.as_str().map(str::to_owned))
    else {
        return SessionBinding::Unbound;
    };
    let (Some(lane_root), Ok(session_root)) = (lane_root, tokio::fs::canonicalize(&root).await)
    else {
        return SessionBinding::Mismatch;
    };
    if session_root == lane_root {
        SessionBinding::Bound
    } else {
        SessionBinding::Mismatch
    }
}

/// `strict` registrations additionally refuse sessions without a workspace.
pub(crate) fn permits(binding: SessionBinding, strict: bool) -> bool {
    match binding {
        SessionBinding::Bound => true,
        SessionBinding::Unbound => !strict,
        SessionBinding::Mismatch => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{Value, json};

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentkit-binding-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    pub(crate) fn open_store() -> SqliteStore {
        let dir = scratch("store");
        SqliteStore::open(dir.join("store.db"), 8).unwrap()
    }

    pub(crate) async fn add_session(store: &SqliteStore, id: &str, workspace: Option<&str>) {
        let mut descriptor = json!({
            "version": 1,
            "profile": {"id": "coding"},
            "createdAt": "2026-01-01T00:00:00.000Z",
        });
        if let Some(root) = workspace {
            descriptor["workspace"] = json!({"root": root, "trusted": true});
        }
        let mut metadata = serde_json::Map::new();
        metadata.insert(agentkit_store::SESSION_METADATA_KEY.to_owned(), descriptor);
        store
            .create_chat(id, None, Value::Object(metadata), 1)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn binding_follows_the_descriptor_workspace() {
        let store = open_store();
        let lane = scratch("lane");
        let other = scratch("other");
        let link = scratch("links").join("alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&lane, &link).unwrap();
        add_session(&store, "a", Some(lane.to_str().unwrap())).await;
        add_session(&store, "b", Some(other.to_str().unwrap())).await;
        add_session(&store, "none", None).await;
        add_session(&store, "gone", Some("/nonexistent/agentkit-binding")).await;
        let root = Some(lane.as_path());
        let bind = |id: &'static str| {
            let store = store.clone();
            async move { session_binding(&store, Some(id), root).await }
        };
        assert_eq!(bind("a").await, SessionBinding::Bound);
        assert_eq!(bind("b").await, SessionBinding::Mismatch);
        assert_eq!(bind("none").await, SessionBinding::Unbound);
        assert_eq!(bind("gone").await, SessionBinding::Mismatch);
        assert_eq!(bind("missing-chat").await, SessionBinding::Unbound);
        assert_eq!(
            session_binding(&store, None, root).await,
            SessionBinding::Unbound
        );
        // An unresolvable lane root cannot match any named workspace.
        assert_eq!(
            session_binding(&store, Some("a"), None).await,
            SessionBinding::Mismatch
        );
        #[cfg(unix)]
        {
            add_session(&store, "via-link", Some(link.to_str().unwrap())).await;
            assert_eq!(bind("via-link").await, SessionBinding::Bound);
        }
    }

    #[test]
    fn permits_matrix() {
        use SessionBinding::*;
        assert!(permits(Bound, false) && permits(Bound, true));
        assert!(permits(Unbound, false) && !permits(Unbound, true));
        assert!(!permits(Mismatch, false) && !permits(Mismatch, true));
    }
}
