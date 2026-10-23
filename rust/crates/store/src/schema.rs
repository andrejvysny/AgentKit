use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

const APPLICATION_ID: i64 = 0x414b5253;
const VERSION: i64 = 2;

fn ownership(path: &Path) -> Result<Option<File>> {
    if path == Path::new(":memory:") {
        return Ok(None);
    }
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        parent.canonicalize()?.join(
            path.file_name()
                .ok_or_else(|| StoreError::InvalidInput("database path has no file name".into()))?,
        )
    };
    let lock_path = ownership_path(&canonical)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock().map_err(|_| StoreError::AlreadyOwned)?;
    Ok(Some(lock))
}

#[cfg(unix)]
fn ownership_path(path: &Path) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // SQLite's platform VFS may share the database's OS lock primitive.
    // A separate inode-keyed lock covers hard-link aliases without interfering.
    let database = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let metadata = database.metadata()?;
    let directory = std::env::temp_dir().join("agentkit-store-owners");
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("{}-{}.lock", metadata.dev(), metadata.ino())))
}

#[cfg(not(unix))]
fn ownership_path(path: &Path) -> Result<std::path::PathBuf> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".agentkit-owner");
    Ok(lock.into())
}

pub(crate) fn open(path: &Path) -> Result<(Connection, Option<File>)> {
    let lock = ownership(path)?;
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let identity: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    let fresh = identity == 0 && version == 0 && count == 0;
    if !fresh && identity != APPLICATION_ID {
        return Err(StoreError::ForeignDatabase);
    }
    if !fresh && ![1, VERSION].contains(&version) {
        return Err(StoreError::UnsupportedVersion(version));
    }
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    if fresh {
        let transaction = connection.transaction()?;
        transaction.execute_batch(&schema_sql(VERSION))?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", VERSION)?;
        transaction.commit()?;
    } else {
        validate_schema(&connection, version)?;
        if version == 1 {
            migrate_events(&mut connection)?;
        }
    }
    if path != Path::new(":memory:") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok((connection, lock))
}

fn validate_schema(connection: &Connection, version: i64) -> Result<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(&schema_sql(version))?;
    if schema_signature(connection)? != schema_signature(&expected)? {
        return Err(StoreError::ForeignDatabase);
    }
    let counter_ok:bool=connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM counters WHERE name='fence' AND typeof(value)='integer' AND value>=0)",
        [],|row|row.get(0),
    )?;
    if !counter_ok {
        return Err(StoreError::ForeignDatabase);
    }
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
    if foreign_keys.query([])?.next()?.is_some() {
        return Err(StoreError::ForeignDatabase);
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::InvalidInput(format!(
            "database integrity: {integrity}"
        )));
    }
    Ok(())
}

fn schema_sql(version: i64) -> String {
    if version == 1 {
        SCHEMA.to_owned()
    } else {
        SCHEMA.replace(
            "seq INTEGER NOT NULL CHECK(seq>=0)",
            "seq REAL NOT NULL CHECK(seq>-1)",
        )
    }
}

fn migrate_events(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch("ALTER TABLE events RENAME TO events_v1;")?;
    transaction.execute_batch("CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq REAL NOT NULL CHECK(seq>-1), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));")?;
    transaction.execute_batch("INSERT INTO events SELECT task_id,seq,event_id,record FROM events_v1; DROP TABLE events_v1;")?;
    transaction.pragma_update(None, "user_version", VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn schema_signature(connection: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    rows.map(|row| row.map_err(Into::into)).collect()
}

const SCHEMA: &str = r#"
CREATE TABLE tasks(id TEXT PRIMARY KEY, scope TEXT NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
CREATE INDEX tasks_scope ON tasks(scope);
CREATE TABLE attempts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE leases(task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO counters VALUES('fence',0);
CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq INTEGER NOT NULL CHECK(seq>=0), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));
CREATE TABLE chats(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE messages(id TEXT PRIMARY KEY, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, parent_id TEXT REFERENCES messages(id), order_key INTEGER NOT NULL, record TEXT NOT NULL, UNIQUE(chat_id,order_key));
CREATE INDEX messages_chat ON messages(chat_id);
CREATE TABLE outbox(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE proposals(id TEXT PRIMARY KEY, scope TEXT NOT NULL, action_id TEXT, status TEXT NOT NULL, operation_id TEXT UNIQUE, record TEXT NOT NULL);
CREATE UNIQUE INDEX proposal_action ON proposals(scope,action_id) WHERE action_id IS NOT NULL AND status NOT IN ('rejected','invalidated');
CREATE TABLE receipts(operation_id TEXT PRIMARY KEY, proposal_id TEXT NOT NULL UNIQUE REFERENCES proposals(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE settings(namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(namespace,key));
CREATE TABLE run_bindings(run_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, scope TEXT NOT NULL, state_required INTEGER NOT NULL DEFAULT 0);
CREATE TABLE continuations(run_id TEXT PRIMARY KEY REFERENCES run_bindings(run_id) ON DELETE CASCADE, anchor_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE, record TEXT NOT NULL);
"#;
