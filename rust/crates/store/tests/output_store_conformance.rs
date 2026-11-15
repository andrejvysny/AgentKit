//! The SQLite adapter is the behavioural reference for the output store
//! port (ADR 0024 §6, ADR 0034): the suite `agentkit-testkit` runs against
//! its in-memory twin runs here too, on a file database with a private
//! output directory. The extra cases cover what only the adapter has:
//! file modes, chat deletion, and tamper detection.

use agentkit_core::{ByteOrLineRange, OutputError, OutputMeta, OutputStore, PurgeReport, Runtime};
use agentkit_store::{SqliteOutputStore, SqliteStore};
use agentkit_testkit::ManualClock;
use agentkit_testkit::conformance::output_store::{self, OutputStoreFixture};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
struct SqliteFixture {
    // Keeps the directory alive for the fixture's clones.
    _directory: Arc<tempfile::TempDir>,
    db_path: PathBuf,
    root: PathBuf,
    sqlite: SqliteStore,
    store: SqliteOutputStore,
    clock: Arc<ManualClock>,
}

impl SqliteFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let db_path = directory.path().join("agentkit.db");
        let root = directory.path().join("outputs");
        let clock = ManualClock::new(output_store::START_MS);
        let sqlite = SqliteStore::open(&db_path, 16)
            .expect("store")
            .with_output_root(&root);
        let store = SqliteOutputStore::new(sqlite.clone(), clock.clone() as Arc<dyn Runtime>)
            .expect("adapter");
        Self {
            _directory: Arc::new(directory),
            db_path,
            root,
            sqlite,
            store,
            clock,
        }
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.root.join(agentkit_kernel::sha256_hex(session))
    }
}

#[async_trait]
impl OutputStoreFixture for SqliteFixture {
    type Store = SqliteOutputStore;

    fn store(&self) -> &SqliteOutputStore {
        &self.store
    }

    fn set_now(&self, now_ms: i64) {
        self.clock.set(now_ms);
    }

    async fn artifacts(&self, session: &str) -> usize {
        match std::fs::read_dir(self.session_dir(session)) {
            Ok(entries) => entries.count(),
            Err(_) => 0,
        }
    }

    async fn link_child(&self, child: &str, parent: &str) {
        let db = rusqlite::Connection::open(&self.db_path).expect("second connection");
        db.busy_timeout(std::time::Duration::from_secs(5))
            .expect("busy timeout");
        db.execute(
            "INSERT INTO subagents(child_chat_id,parent_chat_id,role,depth,prompt,status,created_at,updated_at) VALUES(?,?, 'worker',1,'p','completed',0,0)",
            [child, parent],
        )
        .expect("link");
    }

    async fn purge_expired(&self) -> PurgeReport {
        self.store.purge_expired().await.expect("purge expired")
    }
}

#[tokio::test]
async fn sqlite_output_store_conforms() {
    output_store::run(|| async { SqliteFixture::new() }).await;
}

fn meta(session: &str) -> OutputMeta {
    OutputMeta {
        session: session.into(),
        task: "t".into(),
        call_id: "c".into(),
        tool: "run_command".into(),
        mime: "text/plain".into(),
        exit_code: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn files_and_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = SqliteFixture::new();
    let handle = fixture
        .store
        .put(meta("s1"), b"secret output".to_vec())
        .await
        .expect("put");
    let dir = fixture.session_dir("s1");
    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&fixture.root), 0o700);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(handle.as_str())), 0o600);
}

#[tokio::test]
async fn a_tampered_file_is_refused_not_served() {
    let fixture = SqliteFixture::new();
    let handle = fixture
        .store
        .put(meta("s1"), b"original\n".to_vec())
        .await
        .expect("put");
    std::fs::write(
        fixture.session_dir("s1").join(handle.as_str()),
        b"changed!!\n",
    )
    .expect("tamper");
    let outcome = fixture
        .store
        .read(
            &handle,
            ByteOrLineRange::Lines {
                start_line: 1,
                max_lines: 1,
            },
        )
        .await;
    assert!(
        matches!(outcome, Err(OutputError::Integrity(_))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn deleting_a_chat_removes_its_outputs_file_and_row() {
    let fixture = SqliteFixture::new();
    for chat in ["s1", "s2"] {
        fixture
            .sqlite
            .create_chat(chat, None, json!({}), 1000)
            .await
            .expect("chat");
    }
    let gone = fixture
        .store
        .put(meta("s1"), b"bye".to_vec())
        .await
        .expect("put");
    let kept = fixture
        .store
        .put(meta("s2"), b"stay".to_vec())
        .await
        .expect("put");
    assert_eq!(fixture.artifacts("s1").await, 1);
    fixture.sqlite.delete_chat("s1").await.expect("delete chat");
    assert_eq!(fixture.artifacts("s1").await, 0);
    assert!(!fixture.session_dir("s1").exists());
    assert!(matches!(
        fixture.store.info(&gone).await,
        Err(OutputError::NotFound(_))
    ));
    fixture
        .store
        .info(&kept)
        .await
        .expect("other chat keeps its output");
    assert_eq!(fixture.artifacts("s2").await, 1);
}

#[tokio::test]
async fn the_adapter_needs_an_output_root() {
    let store = SqliteStore::memory(4).expect("store");
    let clock = ManualClock::new(0) as Arc<dyn Runtime>;
    assert!(matches!(
        SqliteOutputStore::new(store, clock),
        Err(OutputError::Invalid(_))
    ));
}

fn raw_output_row(path: &std::path::Path, session: &str) {
    let db = rusqlite::Connection::open(path).expect("raw connection");
    db.busy_timeout(std::time::Duration::from_secs(5))
        .expect("busy timeout");
    db.execute(
        "INSERT INTO tool_outputs(handle,session,task,call_id,tool,mime,byte_len,line_count,sha256,created,expires) VALUES('out_00000000000000000000000000000001',?,'t','c','x','text/plain',1,1,'00',0,9)",
        [session],
    )
    .expect("row");
}

#[tokio::test]
async fn deleting_a_chat_with_outputs_but_no_output_root_is_refused() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agentkit.db");
    let sqlite = SqliteStore::open(&path, 16).expect("store");
    sqlite
        .create_chat("s1", None, json!({}), 1000)
        .await
        .expect("chat");
    raw_output_row(&path, "s1");
    for _ in 0..2 {
        let outcome = sqlite.delete_chat("s1").await;
        assert!(
            matches!(
                outcome,
                Err(agentkit_store::StoreError::OutputsUnavailable(_))
            ),
            "{outcome:?}"
        );
    }
    assert!(
        sqlite.get_chat("s1").await.expect("get").is_some(),
        "nothing was deleted"
    );
}

#[tokio::test]
async fn the_sweep_removes_old_orphan_files_and_keeps_rows_files() {
    let fixture = SqliteFixture::new();
    let kept = fixture
        .store
        .put(meta("s1"), b"kept".to_vec())
        .await
        .expect("put");
    let orphan = fixture
        .session_dir("s1")
        .join("out_00000000000000000000000000000002");
    std::fs::write(&orphan, b"orphan").expect("orphan");
    std::fs::File::options()
        .write(true)
        .open(&orphan)
        .expect("open")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600))
        .expect("age");
    let report = fixture.store.purge_expired().await.expect("sweep");
    assert_eq!(report.outputs, 1);
    assert!(!orphan.exists());
    fixture
        .store
        .read(
            &kept,
            ByteOrLineRange::Lines {
                start_line: 1,
                max_lines: 1,
            },
        )
        .await
        .expect("a row's file stays");
}

#[tokio::test]
async fn a_version_nineteen_database_upgrades_and_keeps_its_rows() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("agentkit.db");
    {
        let sqlite = SqliteStore::open(&path, 16).expect("store");
        for chat in ["c1", "c2"] {
            sqlite
                .create_chat(chat, None, json!({}), 1000)
                .await
                .expect("chat");
        }
    }
    {
        let db = rusqlite::Connection::open(&path).expect("raw connection");
        db.execute(
            "INSERT INTO subagents(child_chat_id,parent_chat_id,role,depth,prompt,status,created_at,updated_at) VALUES('c2','c1','worker',1,'p','completed',0,0)",
            [],
        )
        .expect("subagent row");
        db.execute_batch("DROP TABLE memory_index_items; DROP TABLE memory_fts; DROP TABLE memory_events; DROP TABLE memory_records; DROP TABLE embedding_models; DROP TABLE tool_outputs; PRAGMA user_version=19;")
            .expect("downgrade to 19");
    }
    let sqlite = SqliteStore::open(&path, 16).expect("upgrade");
    assert_eq!(sqlite.list_chats(true).await.expect("chats").len(), 2);
    let db = rusqlite::Connection::open(&path).expect("raw connection");
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, agentkit_store::SCHEMA_VERSION);
    let subagents: i64 = db
        .query_row("SELECT count(*) FROM subagents", [], |row| row.get(0))
        .expect("subagents");
    assert_eq!(subagents, 1);
    raw_output_row(&path, "c1");
    let indexes: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'tool_outputs_%'",
            [],
            |row| row.get(0),
        )
        .expect("indexes");
    assert_eq!(indexes, 2);
}

/// Roadmap 4.1 (DEL-02): a child's chat record lists the handles its brief
/// granted; exactly those become readable, and nothing else changes.
#[tokio::test]
async fn a_recorded_grant_opens_exactly_the_listed_handle() {
    use agentkit_core::SessionTree;
    let fixture = SqliteFixture::new();
    for chat in ["parent", "child", "other-child"] {
        fixture
            .sqlite
            .create_chat(chat, None, json!({}), 1000)
            .await
            .expect("chat");
    }
    let listed = fixture
        .store
        .put(meta("parent"), b"listed".to_vec())
        .await
        .expect("put");
    let unlisted = fixture
        .store
        .put(meta("parent"), b"unlisted".to_vec())
        .await
        .expect("put");
    assert!(
        !fixture
            .store
            .may_read_handle("child", "parent", &listed)
            .await
    );
    fixture
        .sqlite
        .update_chat(
            "child",
            None,
            Some(json!({"agentkit.subagent": {"outputGrants": [listed.as_str()]}})),
            None,
            1001,
        )
        .await
        .expect("grant");
    assert!(
        fixture
            .store
            .may_read_handle("child", "parent", &listed)
            .await
    );
    assert!(
        !fixture
            .store
            .may_read_handle("child", "parent", &unlisted)
            .await
    );
    assert!(
        !fixture
            .store
            .may_read_handle("other-child", "parent", &listed)
            .await
    );
    assert!(
        !fixture.store.may_read("child", "parent").await,
        "a grant is not tree access"
    );
}
