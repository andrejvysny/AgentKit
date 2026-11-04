//! Per-server document store: lazily opened text documents with a
//! content hash and version counter (full-sync `didChange`), a
//! push-diagnostics registry and workspace confinement for file inputs.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::error::{INVALID_FILE, err};
use crate::uri::{path_to_uri, uri_to_path};

/// What the server must be told about a document before a request runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncAction {
    Opened {
        uri: String,
        text: String,
        version: i64,
    },
    Changed {
        uri: String,
        text: String,
        version: i64,
    },
    Unchanged {
        uri: String,
        version: i64,
    },
}

struct DocState {
    path: PathBuf,
    version: i64,
    hash: [u8; 32],
}

/// One store per server generation: opened documents and the latest
/// `publishDiagnostics` payload per URI.
#[derive(Default)]
pub struct DocumentStore {
    docs: Mutex<HashMap<String, DocState>>,
    published: Mutex<HashMap<String, Vec<Value>>>,
    publish_notify: Notify,
}

fn sha256(text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().into()
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the file from disk (UTF-8 required) and reconcile it against
    /// the store, returning the sync action the server needs and the
    /// document version that must guard the request.
    pub fn ensure(&self, path: &Path) -> Result<SyncAction, agentkit_core::CoreError> {
        let uri = path_to_uri(path);
        // Read and hash UNDER the store lock: reading first, then locking,
        // let an interleaved agent edit produce a stale snapshot committed
        // at a HIGHER version — content the server accepts (LSP only
        // rejects decreasing versions) and that the stale-result witness
        // cannot catch, because witness and payload are self-consistent.
        let mut docs = self.docs.lock().expect("docs lock");
        let text = std::fs::read_to_string(path)
            .map_err(|e| err(INVALID_FILE, format!("cannot read {}: {e}", path.display())))?;
        let hash = sha256(&text);
        // The publish-diagnostics registry forgets stale sets when a doc
        // is re-opened after change; the server republishes anyway.
        match docs.get_mut(&uri) {
            None => {
                docs.insert(
                    uri.clone(),
                    DocState {
                        path: path.to_path_buf(),
                        version: 1,
                        hash,
                    },
                );
                self.published.lock().expect("published lock").remove(&uri);
                Ok(SyncAction::Opened {
                    uri,
                    text,
                    version: 1,
                })
            }
            Some(state) if state.hash == hash => Ok(SyncAction::Unchanged {
                uri,
                version: state.version,
            }),
            Some(state) => {
                state.version += 1;
                state.hash = hash;
                self.published.lock().expect("published lock").remove(&uri);
                Ok(SyncAction::Changed {
                    uri,
                    text,
                    version: state.version,
                })
            }
        }
    }

    /// Current version of an open document (the stale-result witness).
    pub fn version(&self, uri: &str) -> Option<i64> {
        self.docs
            .lock()
            .expect("docs lock")
            .get(uri)
            .map(|d| d.version)
    }

    pub fn opened_uris(&self) -> Vec<(String, PathBuf)> {
        self.docs
            .lock()
            .expect("docs lock")
            .iter()
            .map(|(uri, state)| (uri.clone(), state.path.clone()))
            .collect()
    }

    /// Store a `textDocument/publishDiagnostics` payload and wake waiters.
    /// A hostile or broken server must not turn the publish registry into
    /// an unbounded memory sink; a real file's diagnostics never approach
    /// this bound.
    const MAX_PUBLISHED_ITEMS: usize = 2_000;

    pub fn record_publish(&self, uri: &str, mut items: Vec<Value>) {
        if items.len() > Self::MAX_PUBLISHED_ITEMS {
            items.truncate(Self::MAX_PUBLISHED_ITEMS);
        }
        self.published
            .lock()
            .expect("published lock")
            .insert(uri.to_owned(), items);
        self.publish_notify.notify_waiters();
    }

    pub fn published(&self, uri: &str) -> Option<Vec<Value>> {
        self.published
            .lock()
            .expect("published lock")
            .get(uri)
            .cloned()
    }

    /// Wait up to `timeout` for a publish for `uri` (or any update of an
    /// already-published set). Returns the set if one is present at all.
    pub async fn await_publish(&self, uri: &str, timeout: Duration) -> Option<Vec<Value>> {
        if let Some(items) = self.published(uri) {
            return Some(items);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return self.published(uri);
            }
            let notified = self.publish_notify.notified();
            tokio::pin!(notified);
            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return self.published(uri);
            }
            if let Some(items) = self.published(uri) {
                return Some(items);
            }
        }
    }
}

/// Resolve a tool-supplied path against the canonical workspace root:
/// must exist, canonicalize, and stay inside the root (symlink escapes are
/// refused by comparing canonical forms). Returns the canonical path.
pub fn resolve_confined(root_canon: &Path, raw: &str) -> Result<PathBuf, agentkit_core::CoreError> {
    if raw.trim().is_empty() {
        return Err(err(INVALID_FILE, "empty file path"));
    }
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_canon.join(candidate)
    };
    let canonical = joined.canonicalize().map_err(|e| {
        err(
            INVALID_FILE,
            format!("cannot open {}: {e}", joined.display()),
        )
    })?;
    if !canonical.starts_with(root_canon) {
        return Err(err(
            INVALID_FILE,
            format!(
                "{} resolves outside the workspace root {}",
                raw,
                root_canon.display()
            ),
        ));
    }
    std::fs::read_to_string(&canonical).map_err(|e| {
        err(
            INVALID_FILE,
            format!("{} is not readable UTF-8 text: {e}", canonical.display()),
        )
    })?;
    Ok(canonical)
}

/// Render LSP diagnostic items to the normalized tool shape
/// `{file,line,character,endLine?,endCharacter?,severity,code?,source?,message}`.
pub fn normalize_diagnostics(uri: &str, items: &[Value]) -> Vec<Value> {
    let file = uri_to_path(uri)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| uri.to_owned());
    items
        .iter()
        .filter_map(|item| {
            let range = item.get("range")?;
            let start = range.get("start")?;
            let mut out = serde_json::Map::new();
            out.insert("file".into(), json!(file));
            out.insert(
                "line".into(),
                json!(start.get("line").and_then(Value::as_i64).unwrap_or(0) + 1),
            );
            out.insert(
                "character".into(),
                json!(start.get("character").and_then(Value::as_i64).unwrap_or(0) + 1),
            );
            if let Some(end) = range.get("end") {
                if let Some(line) = end.get("line").and_then(Value::as_i64) {
                    out.insert("endLine".into(), json!(line + 1));
                }
                if let Some(ch) = end.get("character").and_then(Value::as_i64) {
                    out.insert("endCharacter".into(), json!(ch + 1));
                }
            }
            out.insert(
                "severity".into(),
                json!(item.get("severity").and_then(Value::as_i64).unwrap_or(4)),
            );
            if let Some(code) = item.get("code").filter(|c| !c.is_null()) {
                out.insert("code".into(), code.clone());
            }
            for key in ["source", "message"] {
                if let Some(value) = item.get(key).filter(|v| v.is_string()) {
                    out.insert(key.into(), value.clone());
                }
            }
            if !out.contains_key("message") {
                out.insert("message".into(), json!(""));
            }
            Some(Value::Object(out))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn open_then_change_increments_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocumentStore::new();
        let path = write(dir.path(), "a.rs", "fn a() {}");
        let opened = store.ensure(&path).unwrap();
        assert_eq!(
            opened,
            SyncAction::Opened {
                uri: path_to_uri(&path),
                text: "fn a() {}".into(),
                version: 1
            }
        );
        let unchanged = store.ensure(&path).unwrap();
        assert!(matches!(
            unchanged,
            SyncAction::Unchanged { version: 1, .. }
        ));
        std::fs::write(&path, "fn a() { 1 }").unwrap();
        let changed = store.ensure(&path).unwrap();
        match changed {
            SyncAction::Changed { version, text, .. } => {
                assert_eq!(version, 2);
                assert_eq!(text, "fn a() { 1 }");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(store.version(&path_to_uri(&path)), Some(2));
    }

    #[test]
    fn non_utf8_file_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.rs");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x80]).unwrap();
        let store = DocumentStore::new();
        let err = store.ensure(&path).unwrap_err();
        assert_eq!(err.code, INVALID_FILE);
    }

    #[test]
    fn confinement_blocks_escapes_and_accepts_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let good = write(&root, "src/a.rs", "x");
        assert_eq!(resolve_confined(&root, "src/a.rs").unwrap(), good);
        assert_eq!(
            resolve_confined(&root, good.to_str().unwrap()).unwrap(),
            good
        );
        assert_eq!(resolve_confined(&root, "./src/../src/a.rs").unwrap(), good);
        assert_eq!(
            resolve_confined(&root, "missing.rs").unwrap_err().code,
            INVALID_FILE
        );
        assert_eq!(
            resolve_confined(&root, "../outside.rs").unwrap_err().code,
            INVALID_FILE
        );
        assert_eq!(
            resolve_confined(&root, "/etc/hostname").unwrap_err().code,
            INVALID_FILE
        );
        // Symlink escape is refused (canonical forms differ).
        std::fs::write(dir.path().join("sneaky.rs"), "x").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret.rs"), root.join("link.rs")).unwrap();
        #[cfg(unix)]
        assert_eq!(
            resolve_confined(&root, "link.rs").unwrap_err().code,
            INVALID_FILE
        );
        assert_eq!(
            resolve_confined(&root, "  ").unwrap_err().code,
            INVALID_FILE
        );
    }

    #[tokio::test]
    async fn publish_await_delivers_late_set() {
        let store = std::sync::Arc::new(DocumentStore::new());
        let uri = "file:///tmp/x.rs";
        let waiter = tokio::spawn({
            let uri = uri.to_owned();
            let store = std::sync::Arc::clone(&store);
            async move { store.await_publish(&uri, Duration::from_secs(5)).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(store.published(uri).is_none());
        store.record_publish(
            uri,
            vec![json!({"range":{"start":{"line":0,"character":0}},"message":"bad"})],
        );
        let items = waiter.await.unwrap().unwrap();
        assert_eq!(items.len(), 1);
        // Immediate return when already present.
        assert!(
            store
                .await_publish(uri, Duration::from_millis(1))
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn publish_await_times_out_to_none() {
        let store = DocumentStore::new();
        assert!(
            store
                .await_publish("file:///tmp/none.rs", Duration::from_millis(30))
                .await
                .is_none()
        );
    }

    #[test]
    fn diagnostics_normalize_with_one_based_positions() {
        let items = vec![
            json!({"range":{"start":{"line":9,"character":4},
                            "end":{"line":9,"character":9}},
                   "severity":1,"code":"E0308","source":"rustc",
                   "message":"mismatched types"}),
            json!({"range":{"start":{"line":0,"character":0}},"message":"hint"}),
            json!({"nope":true}),
        ];
        let norm = normalize_diagnostics("file:///tmp/a%20b.rs", &items);
        assert_eq!(norm.len(), 2);
        assert_eq!(norm[0]["file"], "/tmp/a b.rs");
        assert_eq!(norm[0]["line"], 10);
        assert_eq!(norm[0]["character"], 5);
        assert_eq!(norm[0]["endLine"], 10);
        assert_eq!(norm[0]["endCharacter"], 10);
        assert_eq!(norm[0]["severity"], 1);
        assert_eq!(norm[0]["code"], "E0308");
        assert_eq!(norm[0]["source"], "rustc");
        assert_eq!(norm[0]["message"], "mismatched types");
        assert_eq!(norm[1]["severity"], 4); // default Information→4? spec default hint
        assert!(norm[1].get("endLine").is_none());
    }
}
