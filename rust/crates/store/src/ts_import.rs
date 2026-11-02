//! Import an existing TypeScript `adapters-sqlite` assistant-store database
//! (schema `user_version` 8..=10, `application_id` 0) into this store.
//!
//! The two stores are deliberately mutually exclusive: [`SqliteStore::open`]
//! stamps `application_id` "AKRS" and `user_version` 7 and refuses a foreign
//! marker, while the TypeScript adapters own `application_id` 0 and
//! `user_version` 10. An import therefore never rewrites the source file in
//! place — it READS that file (opened `SQLITE_OPEN_READ_ONLY`) and WRITES a
//! freshly opened Rust store, mapping one row shape onto the other's typed
//! record JSON so the store's own read paths deserialize what lands.
//!
//! What is copied is everything the Rust store can represent losslessly:
//! chats, the message tree (parents first, so the self-referencing foreign key
//! holds), tasks with their attempts, events and leases, run bindings and
//! provider continuations, proposals with their apply receipts, the durable
//! tool-execution receipts — uncertain `admitted` effects included, so the
//! execution guard still refuses to repeat a crash-uncertain mutation after
//! the upgrade —, the outbox, the
//! provider catalogue, the MCP and assistant settings, and the durable agent
//! session inputs and context summaries that both ports spell identically. The
//! shared `fence` and `input_order` counters are pushed past everything the
//! source carried, so a fresh claim or input can never reuse an imported
//! position. What is deliberately NOT copied is derived state — the FTS5
//! `message_search` index and its shadow tables, which the Rust store serves
//! from the message records themselves — and any table the Rust store has no
//! representation for, which is reported in [`ImportReport::warnings`] rather
//! than dropped silently.
//!
//! TypeScript renders every instant as an ISO-8601 string; Rust native clocks
//! are epoch milliseconds. A required instant that will not parse refuses the
//! row; an optional one degrades to absent with a warning, and a chat or
//! message timestamp that will not parse is kept verbatim, which
//! [`ConversationTimestamp`] represents honestly.

use crate::{
    AgentInputRecord, ApplyOutcome, AttemptRecord, ChatRecord, ConversationTimestamp, EventRecord,
    Lease, MessageRecord, OutboxRecord, ProposalRecord, ProposalStatus, ProviderContinuationRecord,
    ReceiptRecord, Result, SqliteStore, StoreError, TaskRecord, TaskStatus,
    context_summaries::CONTEXT_SUMMARIES_NAMESPACE,
    execution_receipts::{ExecutionReceiptRecord, read_execution_receipt},
    provider_catalog::{
        PROVIDER_CAPABILITIES_NAMESPACE, PROVIDER_MODELS_NAMESPACE, PROVIDERS_NAMESPACE,
        capabilities, model, provider,
    },
    util::json,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

/// Where imported MCP server configs live: one settings row per config, keyed
/// by server id, holding the `McpServerConfigRecord` JSON — the same
/// compatibility-namespace convention the provider catalogue uses.
pub const MCP_SERVERS_NAMESPACE: &str = "agentkit.compat.mcp-servers";
/// Where the assistant-wide settings row (the TypeScript `settings` table's
/// single `id = 1` row) lands, as `AssistantSettings` JSON.
pub const ASSISTANT_SETTINGS_NAMESPACE: &str = "agentkit.compat.settings";
/// The only key [`ASSISTANT_SETTINGS_NAMESPACE`] ever holds.
pub const ASSISTANT_SETTINGS_KEY: &str = "assistant";
/// The production assistant-settings document: the namespace/key every
/// runtime reader consumes — `ChatExecutor::stage_tools` (the `toolCalling`
/// staging gate), the HTTP turn lane (default provider/model), the
/// `DesktopBackend` management lane and the napi settings port. Mirroring
/// the compatibility record here is what makes an imported
/// `toolCalling: "off"` reach the executor instead of dying in storage.
/// The literals must match `crates/napi/src/store_settings.rs` and
/// `crates/host/src/http/management.rs`, which hardcode them.
pub const SETTINGS_NAMESPACE: &str = "http";
/// The only key [`SETTINGS_NAMESPACE`] holds the settings document under.
pub const SETTINGS_KEY: &str = "settings";
/// The namespace `McpConfigRepository` reads: ONE JSON array of
/// `McpServerDto`-shaped records, keyed by the application id. The store
/// crate cannot name the host's constructor argument, so promotion takes it
/// through [`ImportOptions::promote_mcp_application_id`].
pub const MCP_CONFIG_NAMESPACE: &str = "mcp-config";

/// The oldest TypeScript schema this importer reads (`MIN_SUPPORTED_SCHEMA_VERSION`).
const MIN_TS_VERSION: i64 = 8;
/// The newest TypeScript schema whose layout this importer was written against.
const KNOWN_TS_VERSION: i64 = 10;

/// What an import copies and how it reacts to a row it cannot map.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Copy `providers`, `provider_models` and `provider_capabilities` into the
    /// namespaces [`SqliteStore::list_providers`] reads.
    pub copy_provider_catalog: bool,
    /// Copy `mcp_servers` into [`MCP_SERVERS_NAMESPACE`].
    pub copy_mcp_servers: bool,
    /// Copy undelivered `outbox` rows.
    pub copy_outbox: bool,
    /// Copy `proposals` and their `proposal_outcomes` (as receipts).
    pub copy_proposals: bool,
    /// Mirror the imported assistant settings into the production settings
    /// document ([`SETTINGS_NAMESPACE`]/[`SETTINGS_KEY`]) that the runtime
    /// readers actually consult — tool-staging policy first among them. The
    /// mirror never overwrites a settings document that already exists.
    pub promote_settings: bool,
    /// Aggregate the imported MCP servers into the single
    /// [`MCP_CONFIG_NAMESPACE`] array document `McpConfigRepository` reads,
    /// keyed by this application id. Promotion refuses to overwrite a live
    /// non-empty document, and quarantines (with a warning) any server the
    /// production reader could not accept.
    pub promote_mcp_application_id: Option<String>,
    /// Stop at the first unmappable row and roll the whole import back. When
    /// false, each bad row becomes a warning and the rest of the database is
    /// still imported.
    pub abort_on_error: bool,
    /// Map, validate and count everything, then roll back without writing.
    pub dry_run: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            copy_provider_catalog: true,
            copy_mcp_servers: true,
            copy_outbox: true,
            copy_proposals: true,
            promote_settings: true,
            promote_mcp_application_id: None,
            abort_on_error: true,
            dry_run: false,
        }
    }
}

/// What an import moved, and everything it could not move.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    /// Chats inserted or refreshed.
    pub chats: usize,
    /// Messages inserted or refreshed.
    pub messages: usize,
    /// Tasks inserted or refreshed.
    pub tasks: usize,
    /// Task events inserted or refreshed.
    pub events: usize,
    /// Attempts inserted or refreshed.
    pub attempts: usize,
    /// Proposals inserted or refreshed.
    pub proposals: usize,
    /// Providers inserted into the compatibility catalogue.
    pub providers: usize,
    /// MCP server configs inserted into the compatibility namespace.
    pub mcp_servers: usize,
    /// 1 when the compatibility settings document was mirrored into the
    /// production settings document (0 when a production document already
    /// existed and was left alone).
    pub promoted_settings: usize,
    /// Servers carried into the production `mcp-config` document; servers the
    /// production reader could not accept are excluded and warned.
    pub promoted_mcp_servers: usize,
    /// Outbox records inserted or refreshed.
    pub outbox: usize,
    /// Apply receipts inserted or refreshed.
    pub receipts: usize,
    /// Durable tool-execution receipts inserted or refreshed — including
    /// `admitted` ones, whose missing result is the honest record of an
    /// uncertain effect the execution guard must keep blocking.
    pub executions: usize,
    /// Durable agent-session inputs inserted or refreshed.
    pub agent_inputs: usize,
    /// Derived context summaries inserted or refreshed.
    pub context_summaries: usize,
    /// Every row or table that could not be imported, and every value that had
    /// to be derived rather than read.
    pub warnings: Vec<String>,
}

/// One source row: every column, with SQL NULL as JSON null.
#[derive(Debug, Default)]
struct Row {
    columns: HashMap<String, Value>,
}

impl Row {
    fn with_capacity(count: usize) -> Self {
        Self {
            columns: HashMap::with_capacity(count),
        }
    }

    fn insert(&mut self, column: String, value: Value) {
        self.columns.insert(column, value);
    }

    fn get(&self, column: &str) -> Option<&Value> {
        self.columns.get(column)
    }

    /// A column as text, coercing the numeric and boolean spellings SQLite's
    /// dynamic typing can legitimately hold in a TEXT column.
    fn text(&self, column: &str) -> Option<String> {
        match self.get(column)? {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            Value::Bool(true) => Some("1".into()),
            Value::Bool(false) => Some("0".into()),
            Value::Number(number) => Some(number.to_string()),
            Value::Array(_) | Value::Object(_) => None,
        }
    }

    fn integer(&self, column: &str) -> Option<i64> {
        match self.get(column)? {
            Value::Number(number) => number.as_i64().or_else(|| {
                number
                    .as_f64()
                    .filter(|value| value.fract() == 0.0)
                    .map(|value| value as i64)
            }),
            Value::Bool(value) => Some(i64::from(*value)),
            Value::String(text) => text.trim().parse().ok(),
            Value::Null | Value::Array(_) | Value::Object(_) => None,
        }
    }

    fn number(&self, column: &str) -> Option<f64> {
        match self.get(column)? {
            Value::Number(number) => number.as_f64(),
            Value::Bool(value) => Some(f64::from(*value)),
            Value::String(text) => text.trim().parse().ok(),
            Value::Null | Value::Array(_) | Value::Object(_) => None,
        }
    }

    /// A SQLite 0/1 flag column.
    fn flag(&self, column: &str) -> Option<bool> {
        match self.get(column)? {
            Value::Null => None,
            Value::Bool(value) => Some(*value),
            Value::Number(number) => number.as_f64().map(|value| value != 0.0),
            Value::String(text) => match text.trim() {
                "0" | "false" | "" => Some(false),
                "1" | "true" => Some(true),
                _ => None,
            },
            Value::Array(_) | Value::Object(_) => None,
        }
    }
}

/// A JavaScript `Date` rendered by `toISOString()`; a bare integer is read as
/// milliseconds, so a store that ever stored one is still readable.
fn instant(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(milliseconds) = text.parse::<i64>() {
        return Some(milliseconds);
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|parsed| parsed.timestamp_millis())
}

impl SqliteStore {
    /// Read a TypeScript adapters-sqlite database and write its contents into
    /// this store, which must be a freshly opened Rust store.
    ///
    /// The source is opened read-only and is never modified. Re-running the
    /// import is safe: every write is an upsert on the destination's primary
    /// key, so a second pass reports the same counts and duplicates nothing.
    /// Importing a database into itself is refused.
    pub async fn import_from_ts_schema(
        &self,
        ts_db_path: &Path,
        opts: &ImportOptions,
    ) -> Result<ImportReport> {
        let source = ts_db_path.to_path_buf();
        let opts = opts.clone();
        self.call(move |db| import(db, &source, &opts)).await
    }
}

fn import(db: &mut Connection, source: &Path, opts: &ImportOptions) -> Result<ImportReport> {
    let canonical = source
        .canonicalize()
        .map_err(|error| StoreError::InvalidImport {
            reason: format!(
                "cannot read the TypeScript database {}: {error}",
                source.display()
            ),
        })?;
    if let Some(destination) = db.path()
        && Path::new(destination)
            .canonicalize()
            .is_ok_and(|path| path == canonical)
    {
        return Err(StoreError::InvalidImport {
            reason: "the TypeScript database is this store's own file".into(),
        });
    }
    let source = Connection::open_with_flags(
        &canonical,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| StoreError::InvalidImport {
        reason: format!(
            "cannot open the TypeScript database {} read-only: {error}",
            canonical.display()
        ),
    })?;
    source.busy_timeout(std::time::Duration::from_secs(5))?;
    let transaction = crate::transaction::atomic(db)?;
    let report = {
        let mut importer = Importer::new(&transaction, source, opts)?;
        importer.run()?;
        importer.report
    };
    // A dry run has written everything inside the transaction, which is then
    // rolled back by simply dropping it: the counts are real, the file is not.
    if !opts.dry_run {
        transaction.commit()?;
    }
    Ok(report)
}

struct Importer<'a> {
    db: &'a Connection,
    source: Connection,
    opts: &'a ImportOptions,
    report: ImportReport,
    /// Set once the receipts' missing `created_at` has been reported, so the
    /// one derivation is announced once instead of per row.
    receipt_stamp_reported: bool,
}

impl<'a> Importer<'a> {
    fn new(db: &'a Connection, source: Connection, opts: &'a ImportOptions) -> Result<Self> {
        let identity: i64 = source
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(|error| StoreError::InvalidImport {
                reason: format!("not a readable SQLite database: {error}"),
            })?;
        if identity == crate::schema::APPLICATION_ID {
            return Err(StoreError::ForeignDatabase);
        }
        if identity != 0 {
            return Err(StoreError::InvalidImport {
                reason: format!("TypeScript stores own application_id 0; found {identity}"),
            });
        }
        let version: i64 = source.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !(MIN_TS_VERSION..=KNOWN_TS_VERSION).contains(&version) {
            return Err(StoreError::InvalidImport {
                reason: format!(
                    "TypeScript schema version {version} is outside the supported \
                     {MIN_TS_VERSION}..={KNOWN_TS_VERSION}"
                ),
            });
        }
        require_table(
            &source,
            "chats",
            &["id", "title", "created_at", "updated_at", "metadata"],
        )?;
        require_table(
            &source,
            "messages",
            &[
                "id",
                "chat_id",
                "run_id",
                "role",
                "content",
                "content_format",
                "order_key",
                "tool_call_id",
                "tool_calls",
                "model_result_json",
                "parent_message_id",
                "depth",
                "branch_index",
                "active",
                "metadata",
                "created_at",
            ],
        )?;
        require_table(
            &source,
            "tasks",
            &[
                "task_id",
                "kind",
                "scope_id",
                "status",
                "priority",
                "enqueued_at",
                "available_at",
                "started_at",
                "finished_at",
                "payload",
                "parent_task_id",
                "depends_on",
                "progress",
                "error",
                "attempt_count",
                "poison_count",
                "dead_lettered_at",
                "dead_letter_reason",
            ],
        )?;
        require_table(
            &source,
            "task_attempts",
            &[
                "attempt_id",
                "task_id",
                "attempt_number",
                "status",
                "owner_id",
                "started_at",
                "ended_at",
                "error",
            ],
        )?;
        require_table(
            &source,
            "leases",
            &[
                "task_id",
                "attempt_id",
                "owner_id",
                "lease_token",
                "fencing_token",
                "expires_at",
            ],
        )?;
        require_table(
            &source,
            "task_events",
            &[
                "task_id",
                "seq",
                "event_id",
                "attempt_id",
                "type",
                "timestamp",
                "payload",
            ],
        )?;
        // One consistent snapshot for every scan: a read transaction held on
        // the source until this connection drops. Without it each table SELECT
        // is its own read transaction, so a still-running legacy writer could
        // move the database between two table scans and the import would mix
        // two generations of rows. A writer that blocks the snapshot for more
        // than the busy timeout makes the import fail closed.
        source.execute_batch("BEGIN")?;
        Ok(Self {
            db,
            source,
            opts,
            report: ImportReport::default(),
            receipt_stamp_reported: false,
        })
    }

    fn run(&mut self) -> Result<()> {
        self.import_chats()?;
        self.import_messages()?;
        self.import_tasks()?;
        self.import_attempts()?;
        self.import_events()?;
        self.import_leases()?;
        self.import_bindings()?;
        self.import_executions()?;
        if self.opts.copy_proposals {
            self.import_proposals()?;
            self.import_outcomes()?;
        }
        if self.opts.copy_outbox {
            self.import_outbox()?;
        }
        if self.opts.copy_provider_catalog {
            self.import_providers()?;
        }
        if self.opts.copy_mcp_servers {
            self.import_mcp_servers()?;
        }
        self.import_settings()?;
        self.import_inputs()?;
        self.import_context_summaries()?;
        self.report_unimported()?;
        Ok(())
    }

    // -- plumbing ----------------------------------------------------------

    /// A row is unmappable: refuse the whole import when asked to, otherwise
    /// record the reason and hand back the caller's "skip this row" signal.
    fn skip<T>(&mut self, reason: impl Into<String>) -> Result<Option<T>> {
        if self.opts.abort_on_error {
            return Err(StoreError::InvalidImport {
                reason: reason.into(),
            });
        }
        self.report.warnings.push(reason.into());
        Ok(None)
    }

    /// The same refusal for a helper that already answers "was it written?".
    fn drop_row(&mut self, reason: impl Into<String>) -> Result<bool> {
        self.skip::<()>(reason)?;
        Ok(false)
    }

    /// A row that cannot be placed at all: refuse, or warn and carry on.
    fn ignore(&mut self, reason: impl Into<String>) -> Result<()> {
        self.skip::<()>(reason)?;
        Ok(())
    }

    /// Run one destination write. A statement error — a constraint the source
    /// violates, most often — aborts or degrades to a warning.
    fn attempt(
        &mut self,
        kind: &str,
        id: &str,
        write: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<bool> {
        match write(self.db) {
            Ok(()) => Ok(true),
            Err(error) if self.opts.abort_on_error => Err(error),
            Err(error) => {
                self.report
                    .warnings
                    .push(format!("{kind} {id} was not imported: {error}"));
                Ok(false)
            }
        }
    }

    fn table_exists(&self, table: &str) -> Result<bool> {
        Ok(self.source.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table],
            |row| row.get(0),
        )?)
    }

    /// Every row of one source table, columns keyed by name.
    fn rows(&self, table: &str) -> Result<Vec<Row>> {
        // Every caller passes a literal table name from this module.
        self.rows_sql(&format!("SELECT * FROM {table}"))
    }

    /// `rows` over an explicit statement; the table name in `sql` is a
    /// literal from this module.
    fn rows_sql(&self, sql: &str) -> Result<Vec<Row>> {
        let mut statement = self.source.prepare(sql)?;
        let names: Vec<String> = statement
            .column_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let mut rows = Vec::new();
        for row in statement.query_map([], |row| {
            let mut values = Row::with_capacity(names.len());
            for (index, name) in names.iter().enumerate() {
                let value = match row.get_ref(index)?.into() {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(number) => Value::from(number),
                    rusqlite::types::Value::Real(number) => serde_json::Number::from_f64(number)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                    rusqlite::types::Value::Text(text) => Value::String(text),
                    // The TypeScript adapters never write a blob.
                    rusqlite::types::Value::Blob(_) => Value::Null,
                };
                values.insert(name.clone(), value);
            }
            Ok(values)
        })? {
            rows.push(row?);
        }
        Ok(rows)
    }

    fn exists(&self, table: &str, key: &str, id: &str) -> Result<bool> {
        // Both names are literals from this module; only `id` is bound.
        Ok(self.db.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {key}=?)"),
            [id],
            |row| row.get(0),
        )?)
    }

    /// A NOT NULL JSON column; `Ok(None)` means the caller skips the row.
    fn json_column(
        &mut self,
        table: &str,
        id: &str,
        row: &Row,
        column: &str,
    ) -> Result<Option<Value>> {
        let Some(text) = Row::text(row, column) else {
            return self.skip(format!("{table} {id}: {column} is not there"));
        };
        match serde_json::from_str(&text) {
            Ok(value) => Ok(Some(value)),
            Err(error) => self.skip(format!("{table} {id}: {column} is not JSON ({error})")),
        }
    }

    /// A nullable JSON column: SQL NULL is an absent field, unparseable text an
    /// absent field with a warning.
    fn optional_json(
        &mut self,
        table: &str,
        id: &str,
        row: &Row,
        column: &str,
    ) -> Result<Option<Value>> {
        let Some(text) = Row::text(row, column) else {
            return Ok(None);
        };
        match serde_json::from_str(&text) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                self.report.warnings.push(format!(
                    "{table} {id}: {column} is not JSON ({error}); the field was dropped"
                ));
                Ok(None)
            }
        }
    }

    /// A required instant on the native epoch-millisecond clock; `Ok(None)`
    /// means the caller skips the row.
    fn millis(&mut self, table: &str, id: &str, column: &str, row: &Row) -> Result<Option<i64>> {
        match Row::text(row, column).as_deref().and_then(instant) {
            Some(milliseconds) => Ok(Some(milliseconds)),
            None => self.skip(format!("{table} {id}: {column} is not an instant")),
        }
    }

    /// An optional instant; an unparseable one is lost with a warning, which is
    /// exactly what an absent optional field means to the Rust record.
    fn opt_millis(
        &mut self,
        table: &str,
        id: &str,
        column: &str,
        row: &Row,
    ) -> Result<Option<i64>> {
        match Row::text(row, column) {
            None => Ok(None),
            Some(text) => match instant(&text) {
                Some(milliseconds) => Ok(Some(milliseconds)),
                None => {
                    self.report.warnings.push(format!(
                        "{table} {id}: {column} is not an ISO instant ({text:?}); imported as absent"
                    ));
                    Ok(None)
                }
            },
        }
    }

    /// A chat or message timestamp, which [`ConversationTimestamp`] can also
    /// hold as opaque text — so an unparseable instant survives verbatim.
    fn timestamp(
        &mut self,
        table: &str,
        id: &str,
        column: &str,
        row: &Row,
    ) -> Result<Option<ConversationTimestamp>> {
        let Some(text) = Row::text(row, column) else {
            return self.skip(format!("{table} {id}: {column} is not there"));
        };
        if let Some(milliseconds) = instant(&text)
            && let Ok(timestamp) = ConversationTimestamp::from_epoch_millis(milliseconds)
        {
            return Ok(Some(timestamp));
        }
        self.report.warnings.push(format!(
            "{table} {id}: {column} is not an ISO instant ({text:?}); kept verbatim"
        ));
        Ok(Some(ConversationTimestamp::Text(text)))
    }

    /// A status column, read through the Rust enum so a vocabulary the store
    /// does not know is refused rather than written as a foreign tag.
    fn status<T: DeserializeOwned>(
        &mut self,
        table: &str,
        id: &str,
        column: &str,
        row: &Row,
    ) -> Result<Option<T>> {
        let Some(text) = Row::text(row, column) else {
            return self.skip(format!("{table} {id}: {column} is not there"));
        };
        match serde_json::from_value(Value::String(text.clone())) {
            Ok(status) => Ok(Some(status)),
            Err(_) => self.skip(format!("{table} {id}: unrecognised {column} {text:?}")),
        }
    }

    /// The indexed status column's own spelling, exactly as the native write
    /// paths store it.
    fn status_text<T: Serialize + agentkit_contracts::JsonDeserialize>(
        &mut self,
        status: &T,
    ) -> Result<String> {
        Ok(json(self.db, status)?.trim_matches('"').to_owned())
    }

    /// An integer column in the destination record's own width; a value that
    /// will not fit is refused, never truncated.
    fn counted<T: DeserializeOwned>(
        &mut self,
        table: &str,
        id: &str,
        column: &str,
        row: &Row,
    ) -> Result<Option<T>> {
        let Some(number) = Row::integer(row, column) else {
            return self.skip(format!("{table} {id}: {column} is not a number"));
        };
        match serde_json::from_value(Value::from(number)) {
            Ok(value) => Ok(Some(value)),
            Err(_) => self.skip(format!("{table} {id}: {column} is out of range")),
        }
    }

    /// One compatibility-namespace settings row, upserted.
    fn setting(&mut self, namespace: &str, key: &str, value: &Value) -> Result<bool> {
        let text = json(self.db, value)?;
        let kind = format!("{namespace} {key}");
        self.attempt(&kind, key, |db| {
            db.execute(
                "INSERT INTO settings(namespace,key,value) VALUES(?,?,?) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value",
                params![namespace, key, text],
            )?;
            Ok(())
        })
    }

    /// One settings row, inserted only when the namespace/key is still free:
    /// a promotion must never overwrite configuration a live deployment
    /// already owns. Returns whether the row was written.
    fn setting_if_absent(&mut self, namespace: &str, key: &str, value: &Value) -> Result<bool> {
        let text = json(self.db, value)?;
        let kind = format!("{namespace} {key}");
        let written = self.attempt(&kind, key, |db| {
            db.execute(
                "INSERT INTO settings(namespace,key,value) VALUES(?,?,?) ON CONFLICT(namespace,key) DO NOTHING",
                params![namespace, key, text],
            )?;
            Ok(())
        })?;
        if !written {
            return Ok(false);
        }
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE namespace=? AND key=? AND value=?)",
            params![namespace, key, text],
            |row| row.get(0),
        )?)
    }

    /// One settings row as parsed JSON. The destination's settings table only
    /// ever holds documents this importer or the store itself wrote, so a
    /// value that does not parse is corruption, not absence.
    fn setting_value(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        let text = self
            .db
            .query_row(
                "SELECT value FROM settings WHERE namespace=? AND key=?",
                params![namespace, key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(text) = text else {
            return Ok(None);
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| StoreError::InvalidImport {
                reason: format!(
                    "the destination settings row {namespace}/{key} is not JSON ({error})"
                ),
            })
    }
}

impl<'a> Importer<'a> {
    // -- conversations -----------------------------------------------------

    fn import_chats(&mut self) -> Result<()> {
        for row in self.rows("chats")? {
            let Some(id) = Row::text(&row, "id") else {
                self.report.warnings.push("chats: a row has no id".into());
                continue;
            };
            let Some(metadata) = self.json_column("chat", &id, &row, "metadata")? else {
                continue;
            };
            let Some(created_at) = self.timestamp("chat", &id, "created_at", &row)? else {
                continue;
            };
            let Some(updated_at) = self.timestamp("chat", &id, "updated_at", &row)? else {
                continue;
            };
            let chat = ChatRecord {
                id: id.clone(),
                title: Row::text(&row, "title"),
                metadata,
                archived: Row::flag(&row, "archived").unwrap_or(false),
                created_at,
                updated_at,
            };
            let record = json(self.db, &chat)?;
            if self.attempt("chat", &id, |db| {
                db.execute(
                    "INSERT INTO chats(id,record) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                    params![id, record],
                )?;
                Ok(())
            })? {
                self.report.chats += 1;
            }
        }
        Ok(())
    }

    /// Parents before children: `messages.parent_id` is a real self-referencing
    /// foreign key, so a row whose parent is not there yet waits for the next
    /// pass. What still waits when progress stops is an orphan or a cycle, and
    /// no number of passes will place it.
    fn import_messages(&mut self) -> Result<()> {
        let mut waiting = self.rows("messages")?;
        loop {
            let mut deferred = Vec::new();
            let mut progress = false;
            for row in waiting.drain(..) {
                let Some(id) = Row::text(&row, "id") else {
                    self.report
                        .warnings
                        .push("messages: a row has no id".into());
                    progress = true;
                    continue;
                };
                let chat_ready = match Row::text(&row, "chat_id") {
                    None => false,
                    Some(chat) => self.exists("chats", "id", &chat)?,
                };
                let parent_ready = match Row::text(&row, "parent_message_id") {
                    None => true,
                    Some(parent) => self.exists("messages", "id", &parent)?,
                };
                if !chat_ready || !parent_ready {
                    deferred.push(row);
                    continue;
                }
                if self.write_message(&id, &row)? {
                    self.report.messages += 1;
                }
                progress = true;
            }
            waiting = deferred;
            if waiting.is_empty() || !progress {
                break;
            }
        }
        for row in waiting {
            let id = Row::text(&row, "id").unwrap_or_default();
            let reason = match Row::text(&row, "parent_message_id") {
                Some(parent) if !self.exists("messages", "id", &parent)? => {
                    format!("message {id}: parent {parent} is not in this database")
                }
                _ => format!(
                    "message {id}: chat {} is not in this database",
                    Row::text(&row, "chat_id").unwrap_or_default()
                ),
            };
            // Strict mode must abort here, never commit an import that
            // silently dropped a message: `skip` answers `Err` precisely
            // when `abort_on_error` demands the whole import be refused.
            self.skip::<()>(reason)?;
        }
        Ok(())
    }

    fn write_message(&mut self, id: &str, row: &Row) -> Result<bool> {
        let Some(chat_id) = Row::text(row, "chat_id") else {
            return self.drop_row(format!("message {id}: chat_id is not there"));
        };
        let Some(role) = Row::text(row, "role") else {
            return self.drop_row(format!("message {id}: role is not there"));
        };
        let Some(body) = Row::text(row, "content") else {
            return self.drop_row(format!("message {id}: content is not there"));
        };
        // `content_format` says how to read the column; guessing would promote
        // a user's JSON-looking string body to parts, exactly what the
        // TypeScript store's comment on that column warns about.
        let format = Row::text(row, "content_format");
        let content = match format.as_deref() {
            Some("text") | None => Value::String(body),
            Some("parts") => match serde_json::from_str::<Value>(&body) {
                Ok(parts) => parts,
                Err(error) => {
                    self.report.warnings.push(format!(
                        "message {id}: parts content is not JSON ({error}); imported as text"
                    ));
                    Value::String(body)
                }
            },
            Some(other) => {
                self.report.warnings.push(format!(
                    "message {id}: unknown content_format {other:?}; imported as text"
                ));
                Value::String(body)
            }
        };
        let Some(order_key) = self.counted::<u64>("message", id, "order_key", row)? else {
            return Ok(false);
        };
        let Some(depth) = self.counted::<u32>("message", id, "depth", row)? else {
            return Ok(false);
        };
        let Some(branch_index) = self.counted::<u32>("message", id, "branch_index", row)? else {
            return Ok(false);
        };
        let Some(created_at) = self.opt_millis("message", id, "created_at", row)? else {
            return self.drop_row(format!("message {id}: created_at is not an instant"));
        };
        let record = MessageRecord {
            id: id.to_owned(),
            chat_id: chat_id.clone(),
            run_id: Row::text(row, "run_id"),
            role,
            content,
            parent_message_id: Row::text(row, "parent_message_id"),
            tool_call_id: Row::text(row, "tool_call_id"),
            tool_calls: self.optional_json("message", id, row, "tool_calls")?,
            model_result_json: Row::text(row, "model_result_json"),
            metadata: self
                .optional_json("message", id, row, "metadata")?
                .unwrap_or_else(|| Value::Object(Map::new())),
            order_key,
            depth,
            branch_index,
            active: Row::flag(row, "active").unwrap_or(true),
            // v8 predates the column; a branch nobody archived is an active one.
            archived: Row::flag(row, "archived").unwrap_or(false),
            created_at: ConversationTimestamp::from_epoch_millis(created_at)?,
        };
        let parent = record.parent_message_id.clone();
        let record = json(self.db, &record)?;
        self.attempt("message", id, |db| {
            db.execute(
                "INSERT INTO messages(id,chat_id,parent_id,order_key,record) VALUES(?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET chat_id=excluded.chat_id, parent_id=excluded.parent_id, order_key=excluded.order_key, record=excluded.record",
                params![id, chat_id, parent, crate::util::sql_integer(order_key)?, record],
            )?;
            Ok(())
        })
    }
}

impl<'a> Importer<'a> {
    // -- tasks -------------------------------------------------------------

    fn import_tasks(&mut self) -> Result<()> {
        for row in self.rows("tasks")? {
            let Some(task_id) = Row::text(&row, "task_id") else {
                self.report
                    .warnings
                    .push("tasks: a row has no task_id".into());
                continue;
            };
            let Some(status) = self.status::<TaskStatus>("task", &task_id, "status", &row)? else {
                continue;
            };
            let Some(payload) = self.json_column("task", &task_id, &row, "payload")? else {
                continue;
            };
            let Some(priority) = self.counted::<i32>("task", &task_id, "priority", &row)? else {
                continue;
            };
            let Some(attempt_count) =
                self.counted::<u32>("task", &task_id, "attempt_count", &row)?
            else {
                continue;
            };
            let Some(poison_count) = self.counted::<u32>("task", &task_id, "poison_count", &row)?
            else {
                continue;
            };
            let Some(enqueued_at) = self.millis("task", &task_id, "enqueued_at", &row)? else {
                continue;
            };
            let Some(available_at) = self.millis("task", &task_id, "available_at", &row)? else {
                continue;
            };
            let depends_on = match self.optional_json("task", &task_id, &row, "depends_on")? {
                None => Vec::new(),
                Some(Value::Array(items)) => {
                    let names: Vec<String> = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect();
                    if names.len() != items.len() {
                        self.report.warnings.push(format!(
                            "task {task_id}: depends_on holds a non-string entry; those entries were dropped"
                        ));
                    }
                    names
                }
                Some(_) => {
                    self.report.warnings.push(format!(
                        "task {task_id}: depends_on is not an array; imported as empty"
                    ));
                    Vec::new()
                }
            };
            let task = TaskRecord {
                task_id: task_id.clone(),
                kind: Row::text(&row, "kind").unwrap_or_default(),
                scope_id: Row::text(&row, "scope_id").unwrap_or_default(),
                status,
                payload,
                priority,
                enqueued_at,
                available_at,
                started_at: self.opt_millis("task", &task_id, "started_at", &row)?,
                finished_at: self.opt_millis("task", &task_id, "finished_at", &row)?,
                depends_on,
                parent_task_id: Row::text(&row, "parent_task_id"),
                attempt_count,
                poison_count,
                dead_lettered_at: self.opt_millis("task", &task_id, "dead_lettered_at", &row)?,
                dead_letter_reason: Row::text(&row, "dead_letter_reason"),
                error: Row::text(&row, "error"),
                progress: self.optional_json("task", &task_id, &row, "progress")?,
                // The TypeScript tasks table has no column for any of these, so
                // recording them as their default is honest, not a loss.
                cancel_requested: false,
                runtime_state: None,
                retry_pending: false,
                interrupt_requested: false,
            };
            let status = self.status_text(&task.status)?;
            let scope = task.scope_id.clone();
            let record = json(self.db, &task)?;
            if self.attempt("task", &task_id, |db| {
                db.execute(
                    "INSERT INTO tasks(id,scope,status,record) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET scope=excluded.scope, status=excluded.status, record=excluded.record",
                    params![task_id, scope, status, record],
                )?;
                Ok(())
            })? {
                self.report.tasks += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    fn import_attempts(&mut self) -> Result<()> {
        for row in self.rows("task_attempts")? {
            let Some(attempt_id) = Row::text(&row, "attempt_id") else {
                self.report
                    .warnings
                    .push("task_attempts: a row has no attempt_id".into());
                continue;
            };
            let Some(task_id) = Row::text(&row, "task_id") else {
                self.ignore(format!("attempt {attempt_id}: task_id is not there"))?;
                continue;
            };
            if !self.exists("tasks", "id", &task_id)? {
                self.ignore(format!(
                    "attempt {attempt_id}: its task {task_id} was not imported"
                ))?;
                continue;
            }
            let Some(attempt_number) =
                self.counted::<u32>("attempt", &attempt_id, "attempt_number", &row)?
            else {
                continue;
            };
            let Some(status) = Row::text(&row, "status") else {
                self.ignore(format!("attempt {attempt_id}: status is not there"))?;
                continue;
            };
            let Some(started_at) = self.millis("attempt", &attempt_id, "started_at", &row)? else {
                continue;
            };
            let attempt = AttemptRecord {
                attempt_id: attempt_id.clone(),
                task_id: task_id.clone(),
                attempt_number,
                status,
                owner_id: Row::text(&row, "owner_id").unwrap_or_default(),
                started_at,
                ended_at: self.opt_millis("attempt", &attempt_id, "ended_at", &row)?,
                error: Row::text(&row, "error"),
            };
            let record = json(self.db, &attempt)?;
            if self.attempt("attempt", &attempt_id, |db| {
                db.execute(
                    "INSERT INTO attempts(id,task_id,record) VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET task_id=excluded.task_id, record=excluded.record",
                    params![attempt_id, task_id, record],
                )?;
                Ok(())
            })? {
                self.report.attempts += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// `task_events` shreds the envelope across columns and keeps the whole
    /// event in `payload`; the Rust record wants the envelope itself, which is
    /// exactly what `payload` already holds. Only a row whose payload cannot
    /// answer for itself is rebuilt from the columns, and that is reported.
    fn import_events(&mut self) -> Result<()> {
        for row in self.rows("task_events")? {
            let Some(task_id) = Row::text(&row, "task_id") else {
                self.report
                    .warnings
                    .push("task_events: a row has no task_id".into());
                continue;
            };
            let Some(event_id) = Row::text(&row, "event_id") else {
                self.report.warnings.push(format!(
                    "task_events: a row of task {task_id} has no event_id"
                ));
                continue;
            };
            if !self.exists("tasks", "id", &task_id)? {
                self.ignore(format!(
                    "event {event_id}: its task {task_id} was not imported"
                ))?;
                continue;
            }
            let Some(seq) = Row::number(&row, "seq").filter(|seq| *seq >= 0.0) else {
                self.ignore(format!(
                    "event {event_id}: seq is not a non-negative number"
                ))?;
                continue;
            };
            let event = match self.json_column("event", &event_id, &row, "payload")? {
                None => continue,
                Some(event @ Value::Object(_)) => event,
                Some(_) => {
                    self.report.warnings.push(format!(
                        "event {event_id}: payload is not an object; rebuilt from the columns"
                    ));
                    rebuilt_event(&row, seq, &event_id)
                }
            };
            let Some(created_at) = self.millis("event", &event_id, "timestamp", &row)? else {
                continue;
            };
            let record = EventRecord {
                task_id: task_id.clone(),
                seq,
                event,
                created_at,
            };
            let record = json(self.db, &record)?;
            if self.attempt("event", &event_id, |db| {
                db.execute(
                    "INSERT INTO events(task_id,seq,event_id,record) VALUES(?,?,?,?) ON CONFLICT(task_id,seq) DO UPDATE SET event_id=excluded.event_id, record=excluded.record",
                    params![task_id, seq, event_id, record],
                )?;
                Ok(())
            })? {
                self.report.events += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// A lease only means something while its task does, and a fencing token
    /// must never be handed out twice: the store-global counter is pushed past
    /// every token this import brings in, so a worker still holding an imported
    /// lease cannot be imitated by a freshly minted one.
    fn import_leases(&mut self) -> Result<()> {
        let mut highest = 0_i64;
        for row in self.rows("leases")? {
            let Some(task_id) = Row::text(&row, "task_id") else {
                self.report
                    .warnings
                    .push("leases: a row has no task_id".into());
                continue;
            };
            if !self.exists("tasks", "id", &task_id)? {
                self.ignore(format!("lease of {task_id}: its task was not imported"))?;
                continue;
            }
            let Some(fencing_token) =
                Row::integer(&row, "fencing_token").filter(|token| *token >= 0)
            else {
                self.ignore(format!(
                    "lease of {task_id}: fencing_token is not a non-negative integer"
                ))?;
                continue;
            };
            let Some(expires_at) = self.millis("lease", &task_id, "expires_at", &row)? else {
                continue;
            };
            let lease = Lease {
                task_id: task_id.clone(),
                attempt_id: Row::text(&row, "attempt_id").unwrap_or_default(),
                owner_id: Row::text(&row, "owner_id").unwrap_or_default(),
                lease_token: Row::text(&row, "lease_token").unwrap_or_default(),
                fencing_token: fencing_token as u64,
                expires_at,
            };
            highest = highest.max(fencing_token);
            let record = json(self.db, &lease)?;
            self.attempt("lease", &task_id, |db| {
                db.execute(
                    "INSERT INTO leases(task_id,record) VALUES(?,?) ON CONFLICT(task_id) DO UPDATE SET record=excluded.record",
                    params![task_id, record],
                )?;
                Ok(())
            })?;
        }
        if self.table_exists("fencing_counter")? {
            let value = self
                .source
                .query_row("SELECT value FROM fencing_counter WHERE id=1", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
            if let Some(value) = value {
                highest = highest.max(value);
            }
        }
        self.counter("fence", highest)
    }
}

impl<'a> Importer<'a> {
    /// `provider_run_scopes` and `provider_continuations` land in the Rust
    /// `run_bindings` and `continuations` tables. Both destinations have real
    /// foreign keys (task, chat, anchor message) and both are read back through
    /// the strict continuation validator, so a row that would not survive that
    /// read is refused here rather than written into a store that then cannot
    /// answer for it.
    fn import_bindings(&mut self) -> Result<()> {
        if self.table_exists("provider_run_scopes")? {
            for row in self.rows("provider_run_scopes")? {
                let Some(run_id) = Row::text(&row, "run_id") else {
                    self.report
                        .warnings
                        .push("provider_run_scopes: a row has no run_id".into());
                    continue;
                };
                if !self.exists("tasks", "id", &run_id)? {
                    self.ignore(format!("run binding {run_id}: its task was not imported"))?;
                    continue;
                }
                let Some(scope) = self.json_column("run binding", &run_id, &row, "scope_json")?
                else {
                    continue;
                };
                if crate::data::continuation_validation::scope(&scope).is_err() {
                    self.ignore(format!(
                        "run binding {run_id}: scope is not a valid provider continuation scope"
                    ))?;
                    continue;
                }
                let chat_id = scope["chatId"].as_str().unwrap_or_default().to_owned();
                if !self.exists("chats", "id", &chat_id)? {
                    self.ignore(format!(
                        "run binding {run_id}: its chat {chat_id} was not imported"
                    ))?;
                    continue;
                }
                let state_required = i64::from(Row::flag(&row, "state_required").unwrap_or(false));
                let scope = json(self.db, &scope)?;
                self.attempt("run binding", &run_id, |db| {
                    db.execute(
                        "INSERT INTO run_bindings(run_id,chat_id,scope,state_required) VALUES(?,?,?,?) ON CONFLICT(run_id) DO UPDATE SET chat_id=excluded.chat_id, scope=excluded.scope, state_required=excluded.state_required",
                        params![run_id, chat_id, scope, state_required],
                    )?;
                    Ok(())
                })?;
            }
        }
        if self.table_exists("provider_continuations")? {
            for row in self.rows("provider_continuations")? {
                let Some(run_id) = Row::text(&row, "run_id") else {
                    self.report
                        .warnings
                        .push("provider_continuations: a row has no run_id".into());
                    continue;
                };
                let Some(anchor_id) = Row::text(&row, "anchor_message_id") else {
                    self.ignore(format!("continuation {run_id}: no anchor message"))?;
                    continue;
                };
                let Some(state) = self.json_column("continuation", &run_id, &row, "state_json")?
                else {
                    continue;
                };
                let chat_id = match Row::text(&row, "chat_id") {
                    Some(chat) => chat,
                    None => state["scope"]["chatId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                };
                // The binding's foreign key needs the task and the chat; a run
                // whose scope row was never written gets one from its state.
                if !self.exists("tasks", "id", &run_id)?
                    || !self.exists("chats", "id", &chat_id)?
                    || !self.exists("messages", "id", &anchor_id)?
                {
                    self.ignore(format!(
                        "continuation {run_id}: its task, chat {chat_id}, or anchor {anchor_id} is missing"
                    ))?;
                    continue;
                }
                if !self.exists("run_bindings", "run_id", &run_id)? {
                    let scope = state.get("scope").cloned().unwrap_or(Value::Null);
                    if crate::data::continuation_validation::scope(&scope).is_err() {
                        self.ignore(format!(
                            "continuation {run_id}: its state carries no valid scope to bind"
                        ))?;
                        continue;
                    }
                    let scope = json(self.db, &scope)?;
                    self.attempt("run binding", &run_id, |db| {
                        db.execute(
                            "INSERT INTO run_bindings(run_id,chat_id,scope,state_required) VALUES(?,?,?,1)",
                            params![run_id, chat_id, scope],
                        )?;
                        Ok(())
                    })?;
                }
                let record = ProviderContinuationRecord {
                    run_id: run_id.clone(),
                    anchor_message_id: anchor_id.clone(),
                    state,
                };
                if crate::data::continuation_validation::stored(self.db, &record).is_err() {
                    self.ignore(format!(
                        "continuation {run_id}: its state does not match the run's binding or anchor"
                    ))?;
                    continue;
                }
                let text = json(self.db, &record)?;
                self.attempt("continuation", &run_id, |db| {
                    db.execute(
                        "INSERT INTO continuations(run_id,anchor_id,record) VALUES(?,?,?) ON CONFLICT(run_id) DO UPDATE SET anchor_id=excluded.anchor_id, record=excluded.record",
                        params![run_id, anchor_id, text],
                    )?;
                    db.execute(
                        "UPDATE run_bindings SET state_required=1 WHERE run_id=?",
                        [&run_id],
                    )?;
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// `tool_executions` rows ARE importable, and dropping them is unsafe:
    /// the execution guard replays completed results and refuses reissue of
    /// an `admitted`-without-result effect — protection that must survive the
    /// upgrade or a crash-uncertain mutation can be executed twice. The only
    /// field the TypeScript record lacks is `chatId`; it is re-derived from
    /// what the import itself wrote (the run's binding, else the unique chat
    /// whose messages carry this run), and a receipt whose chat cannot be
    /// proven is quarantined: strict mode refuses the whole import, tolerant
    /// mode refuses the row with a loud warning, so the barrier is never
    /// silently weakened. Rows are scanned in rowid order and written in the
    /// same order, because the guard's replay barrier treats the NEWEST
    /// receipt for a tool call as the one that counts.
    fn import_executions(&mut self) -> Result<()> {
        if !self.table_exists("tool_executions")? {
            return Ok(());
        }
        for row in self.rows_sql(
            "SELECT execution_id, run_id, approval_request_id, record \
             FROM tool_executions ORDER BY rowid",
        )? {
            let Some(execution_id) = Row::text(&row, "execution_id") else {
                self.report
                    .warnings
                    .push("tool_executions: a row has no execution_id".into());
                continue;
            };
            let Some(run_id) = Row::text(&row, "run_id") else {
                self.ignore(format!("execution {execution_id}: run_id is not there"))?;
                continue;
            };
            if !self.exists("tasks", "id", &run_id)? {
                self.ignore(format!(
                    "execution {execution_id}: its run {run_id} was not imported"
                ))?;
                continue;
            }
            let Some(mut record) =
                self.json_column("tool_execution", &execution_id, &row, "record")?
            else {
                continue;
            };
            let Value::Object(fields) = &mut record else {
                self.ignore(format!("execution {execution_id}: record is not an object"))?;
                continue;
            };
            let chat_id = match self.chat_of_run(&run_id)? {
                Some(chat) => {
                    if let Some(stored) = fields.get("chatId")
                        && stored.as_str() != Some(chat.as_str())
                    {
                        self.report.warnings.push(format!(
                            "tool_execution {execution_id}: its record claims chat \
                             {stored} but the run provably belongs to {chat}; the \
                             proven chat was kept"
                        ));
                    }
                    chat
                }
                None => {
                    self.ignore(format!(
                        "execution {execution_id}: run {run_id} binds to no single \
                         chat, so the chat-scoped uncertainty barrier cannot be \
                         rebuilt for this receipt"
                    ))?;
                    continue;
                }
            };
            fields.insert("chatId".into(), Value::String(chat_id));
            let parsed = match serde_json::from_value::<ExecutionReceiptRecord>(record) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.ignore(format!(
                        "execution {execution_id}: the record is not an execution \
                         receipt ({error})"
                    ))?;
                    continue;
                }
            };
            // The destination's own anti-corruption validator gates the write:
            // a receipt that would make a later recovery read fail closed must
            // never enter the store at all.
            if let Err(error) = read_execution_receipt(
                serde_json::to_value(&parsed).map_err(|error| StoreError::InvalidImport {
                    reason: format!("execution {execution_id}: cannot re-encode ({error})"),
                })?,
                &execution_id,
                &run_id,
                Some(Row::text(&row, "approval_request_id").as_deref()),
            ) {
                self.ignore(format!("execution {execution_id}: {error}"))?;
                continue;
            }
            let approval_request_id = Row::text(&row, "approval_request_id");
            let text = json(self.db, &parsed)?;
            if self.attempt("execution", &execution_id, |db| {
                db.execute(
                    "INSERT INTO tool_executions(execution_id,run_id,approval_request_id,record) \
                     VALUES(?,?,?,?) ON CONFLICT(execution_id) DO UPDATE SET \
                     run_id=excluded.run_id, approval_request_id=excluded.approval_request_id, \
                     record=excluded.record",
                    params![execution_id, run_id, approval_request_id, text],
                )?;
                Ok(())
            })? {
                self.report.executions += 1;
            }
        }
        Ok(())
    }

    /// The one chat a run belongs to: its imported provider binding first,
    /// else the unique chat whose imported messages carry this run — the
    /// same two sources the Rust v3 receipt backfill resolves through.
    /// Ambiguity (two chats) is answered as no chat: never a guess.
    fn chat_of_run(&self, run_id: &str) -> Result<Option<String>> {
        let bound = self
            .db
            .query_row(
                "SELECT chat_id FROM run_bindings WHERE run_id=?",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(chat) = bound.filter(|chat| !chat.is_empty()) {
            return Ok(Some(chat));
        }
        let mut statement = self.db.prepare(
            "SELECT DISTINCT chat_id FROM messages WHERE json_extract(record,'$.runId')=?1",
        )?;
        let chats: Vec<String> = statement
            .query_map([run_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(if chats.len() == 1 {
            chats.into_iter().next()
        } else {
            None
        })
    }
}

impl<'a> Importer<'a> {
    fn import_proposals(&mut self) -> Result<()> {
        if !self.table_exists("proposals")? {
            return Ok(());
        }
        for row in self.rows("proposals")? {
            let Some(id) = Row::text(&row, "id") else {
                self.report
                    .warnings
                    .push("proposals: a row has no id".into());
                continue;
            };
            let Some(status) = self.status::<ProposalStatus>("proposal", &id, "status", &row)?
            else {
                continue;
            };
            let Some(envelope) = self.json_column("proposal", &id, &row, "envelope")? else {
                continue;
            };
            let Some(operations) = self.json_column("proposal", &id, &row, "operations")? else {
                continue;
            };
            let operations = match operations {
                Value::Array(items) => items,
                _ => {
                    self.ignore(format!("proposal {id}: operations is not an array"))?;
                    continue;
                }
            };
            let warnings = match self.optional_json("proposal", &id, &row, "warnings")? {
                Some(Value::Array(items)) => {
                    let noted: Vec<String> = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect();
                    if noted.len() != items.len() {
                        self.report.warnings.push(format!(
                            "proposal {id}: warnings holds a non-string entry; those entries were dropped"
                        ));
                    }
                    noted
                }
                Some(_) => {
                    self.report.warnings.push(format!(
                        "proposal {id}: warnings is not an array; imported as empty"
                    ));
                    Vec::new()
                }
                None => {
                    self.report.warnings.push(format!(
                        "proposal {id}: warnings is not there; imported as empty"
                    ));
                    Vec::new()
                }
            };
            let Some(created_at) = self.millis("proposal", &id, "created_at", &row)? else {
                continue;
            };
            let record = ProposalRecord {
                id: id.clone(),
                chat_id: Row::text(&row, "chat_id").unwrap_or_default(),
                run_id: Row::text(&row, "run_id"),
                scope_key: Row::text(&row, "scope_key").unwrap_or_default(),
                action_id: Row::text(&row, "action_id"),
                tool_name: Row::text(&row, "tool_name").unwrap_or_default(),
                kind: Row::text(&row, "kind").unwrap_or_default(),
                risk: Row::text(&row, "risk").unwrap_or_default(),
                status,
                envelope,
                operations,
                warnings,
                truncated: Row::flag(&row, "truncated").unwrap_or(false),
                revision_at_create: Row::text(&row, "revision_at_create"),
                operation_id: Row::text(&row, "operation_id"),
                decision: self.optional_json("proposal", &id, &row, "decision")?,
                reason: Row::text(&row, "reason"),
                created_at,
                decided_at: self.opt_millis("proposal", &id, "decided_at", &row)?,
                claimed_at: self.opt_millis("proposal", &id, "claimed_at", &row)?,
                applied_at: self.opt_millis("proposal", &id, "applied_at", &row)?,
            };
            let status = self.status_text(&record.status)?;
            let (scope, action_id, operation_id) = (
                record.scope_key.clone(),
                record.action_id.clone(),
                record.operation_id.clone(),
            );
            let text = json(self.db, &record)?;
            if self.attempt("proposal", &id, |db| {
                db.execute(
                    "INSERT INTO proposals(id,scope,action_id,status,operation_id,record) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET scope=excluded.scope, action_id=excluded.action_id, status=excluded.status, operation_id=excluded.operation_id, record=excluded.record",
                    params![id, scope, action_id, status, operation_id, text],
                )?;
                Ok(())
            })? {
                self.report.proposals += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// `proposal_outcomes` becomes the Rust `receipts` table. The TypeScript
    /// outcome row carries no proposal id and no instant of its own: it is
    /// linked through `proposals.operation_id`, and its timestamp is the owning
    /// proposal's own last stamp — the derivation is reported once.
    fn import_outcomes(&mut self) -> Result<()> {
        if !self.table_exists("proposal_outcomes")? {
            return Ok(());
        }
        for row in self.rows("proposal_outcomes")? {
            let Some(operation_id) = Row::text(&row, "operation_id") else {
                self.report
                    .warnings
                    .push("proposal_outcomes: a row has no operation_id".into());
                continue;
            };
            let Some(status) = Row::text(&row, "status") else {
                self.ignore(format!("receipt {operation_id}: status is not there"))?;
                continue;
            };
            let Some(applied_ops) =
                self.counted::<u32>("receipt", &operation_id, "applied_ops", &row)?
            else {
                continue;
            };
            let failed_ops = match self.json_column("receipt", &operation_id, &row, "failed_ops")? {
                Some(Value::Array(items)) => items,
                Some(_) => {
                    self.ignore(format!(
                        "receipt {operation_id}: failed_ops is not an array"
                    ))?;
                    continue;
                }
                None => continue,
            };
            let owner = self
                .source
                .query_row(
                    "SELECT id, created_at, decided_at, applied_at FROM proposals WHERE operation_id=?",
                    [&operation_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((proposal_id, created_at, decided_at, applied_at)) = owner else {
                self.ignore(format!(
                    "receipt {operation_id}: no proposal carries this operation_id"
                ))?;
                continue;
            };
            if !self.exists("proposals", "id", &proposal_id)? {
                self.ignore(format!(
                    "receipt {operation_id}: its proposal {proposal_id} was not imported"
                ))?;
                continue;
            }
            if !self.receipt_stamp_reported {
                self.receipt_stamp_reported = true;
                self.report.warnings.push(
                    "receipts: the TypeScript proposal_outcomes table stores no instant, so created_at is the owning proposal's applied_at, decided_at, or created_at"
                        .into(),
                );
            }
            let Some(created_at) = applied_at
                .as_deref()
                .or(decided_at.as_deref())
                .or(Some(created_at.as_str()))
                .and_then(instant)
            else {
                self.ignore(format!(
                    "receipt {operation_id}: the owning proposal carries no parsable instant"
                ))?;
                continue;
            };
            let record = ReceiptRecord {
                proposal_id: proposal_id.clone(),
                operation_id: operation_id.clone(),
                outcome: ApplyOutcome {
                    status,
                    applied_ops,
                    failed_ops,
                    result_json: Row::text(&row, "result_json"),
                    revision: Row::text(&row, "revision"),
                },
                created_at,
            };
            let text = json(self.db, &record)?;
            if self.attempt("receipt", &operation_id, |db| {
                db.execute(
                    "INSERT INTO receipts(operation_id,proposal_id,record) VALUES(?,?,?) ON CONFLICT(operation_id) DO UPDATE SET proposal_id=excluded.proposal_id, record=excluded.record",
                    params![operation_id, proposal_id, text],
                )?;
                Ok(())
            })? {
                self.report.receipts += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// Undelivered outbox rows. The TypeScript table has no claim columns, so a
    /// record arrives unclaimed — which is exactly what an unclaimed row is.
    fn import_outbox(&mut self) -> Result<()> {
        if !self.table_exists("outbox")? {
            return Ok(());
        }
        for row in self.rows("outbox")? {
            let Some(id) = Row::text(&row, "id") else {
                self.report.warnings.push("outbox: a row has no id".into());
                continue;
            };
            let Some(payload) = self.json_column("outbox", &id, &row, "payload")? else {
                continue;
            };
            let Some(created_at) = self.millis("outbox", &id, "created_at", &row)? else {
                continue;
            };
            let Some(available_at) = self.millis("outbox", &id, "available_at", &row)? else {
                continue;
            };
            let Some(attempts) = self.counted::<u32>("outbox", &id, "attempts", &row)? else {
                continue;
            };
            let record = OutboxRecord {
                id: id.clone(),
                topic: Row::text(&row, "topic").unwrap_or_default(),
                run_id: Row::text(&row, "run_id"),
                payload,
                created_at,
                available_at,
                attempts,
                published_at: self.opt_millis("outbox", &id, "published_at", &row)?,
                last_error: Row::text(&row, "last_error"),
                claim_token: None,
                claim_owner: None,
                claim_until: None,
            };
            let text = json(self.db, &record)?;
            if self.attempt("outbox", &id, |db| {
                db.execute(
                    "INSERT INTO outbox(id,record) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                    params![id, text],
                )?;
                Ok(())
            })? {
                self.report.outbox += 1;
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// `providers`, `provider_models` and `provider_capabilities` are shredded
    /// columns; the Rust catalogue keeps one JSON record per provider in the
    /// settings table. Each record is checked with the very validator the read
    /// path applies, so an import can never leave a catalogue behind that
    /// [`SqliteStore::list_providers`] refuses to serve.
    fn import_providers(&mut self) -> Result<()> {
        if self.table_exists("providers")? {
            for row in self.rows("providers")? {
                let Some(id) = Row::text(&row, "id") else {
                    self.report
                        .warnings
                        .push("providers: a row has no id".into());
                    continue;
                };
                let Some(record) = self.provider_record(&id, &row)? else {
                    continue;
                };
                if self.setting(PROVIDERS_NAMESPACE, &id, &record)? {
                    self.report.providers += 1;
                }
            }
        }
        self.import_provider_models()?;
        self.import_provider_capabilities()
    }

    fn provider_record(&mut self, id: &str, row: &Row) -> Result<Option<Value>> {
        let mut record = Map::new();
        record.insert("id".into(), Value::String(id.to_owned()));
        for (field, column) in [
            ("label", "label"),
            ("kind", "kind"),
            ("baseUrl", "base_url"),
            ("defaultModel", "default_model"),
        ] {
            let Some(text) = Row::text(row, column) else {
                return self.skip(format!("provider {id}: {column} is not there"));
            };
            record.insert(field.into(), Value::String(text));
        }
        if let Some(key) = Row::text(row, "api_key") {
            record.insert("apiKey".into(), Value::String(key));
        }
        let Some(enabled) = Row::flag(row, "enabled") else {
            return self.skip(format!("provider {id}: enabled is not a flag"));
        };
        record.insert("enabled".into(), Value::Bool(enabled));
        for (field, column) in [("extraHeaders", "extra_headers"), ("metadata", "metadata")] {
            if let Some(value) = self.optional_json("provider", id, row, column)? {
                if value.is_object() {
                    record.insert(field.into(), value);
                } else {
                    self.report.warnings.push(format!(
                        "provider {id}: {column} is not an object; the field was dropped"
                    ));
                }
            }
        }
        let record = Value::Object(record);
        if !provider(&record) {
            return self.skip(format!("provider {id}: the record is malformed"));
        }
        Ok(Some(record))
    }

    fn import_provider_models(&mut self) -> Result<()> {
        if !self.table_exists("provider_models")? {
            return Ok(());
        }
        let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
        for row in self.rows("provider_models")? {
            let Some(provider_id) = Row::text(&row, "provider_id") else {
                self.report
                    .warnings
                    .push("provider_models: a row has no provider_id".into());
                continue;
            };
            let Some(model_id) = Row::text(&row, "model_id") else {
                self.ignore(format!("model of {provider_id}: model_id is not there"))?;
                continue;
            };
            let Some(fetched_at) = Row::text(&row, "fetched_at") else {
                self.ignore(format!("model {model_id}: fetched_at is not there"))?;
                continue;
            };
            let mut record = Map::new();
            record.insert("providerId".into(), Value::String(provider_id.clone()));
            record.insert("modelId".into(), Value::String(model_id));
            record.insert(
                "displayName".into(),
                Row::text(&row, "display_name")
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            record.insert("fetchedAt".into(), Value::String(fetched_at));
            if let Some(tokens) = Row::integer(&row, "context_window_tokens") {
                record.insert("contextWindowTokens".into(), Value::from(tokens));
            }
            if let Some(supported) = Row::flag(&row, "supports_tool_calling") {
                record.insert("supportsToolCalling".into(), Value::Bool(supported));
            }
            let record = Value::Object(record);
            if !model(&record) {
                self.ignore(format!(
                    "model {} of {provider_id}: the record is malformed",
                    record["modelId"].as_str().unwrap_or_default()
                ))?;
                continue;
            }
            if !self.setting_exists(PROVIDERS_NAMESPACE, &provider_id)? {
                self.ignore(format!(
                    "model {} of {provider_id}: its provider was not imported",
                    record["modelId"].as_str().unwrap_or_default()
                ))?;
                continue;
            }
            grouped.entry(provider_id).or_default().push(record);
        }
        for (provider_id, models) in grouped {
            self.setting(
                PROVIDER_MODELS_NAMESPACE,
                &provider_id,
                &Value::Array(models),
            )?;
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    fn import_provider_capabilities(&mut self) -> Result<()> {
        if !self.table_exists("provider_capabilities")? {
            return Ok(());
        }
        for row in self.rows("provider_capabilities")? {
            let Some(provider_id) = Row::text(&row, "provider_id") else {
                self.report
                    .warnings
                    .push("provider_capabilities: a row has no provider_id".into());
                continue;
            };
            let mut record = Map::new();
            for (field, column) in [
                ("streaming", "streaming"),
                ("toolCalling", "tool_calling"),
                ("modelList", "model_list"),
            ] {
                let Some(flag) = Row::flag(&row, column) else {
                    self.ignore(format!(
                        "capabilities of {provider_id}: {column} is not a flag"
                    ))?;
                    break;
                };
                record.insert(field.into(), Value::Bool(flag));
            }
            if record.len() != 3 {
                continue;
            }
            for (field, column) in [("vision", "vision"), ("jsonMode", "json_mode")] {
                if let Some(flag) = Row::flag(&row, column) {
                    record.insert(field.into(), Value::Bool(flag));
                }
            }
            if let Some(tokens) = Row::integer(&row, "max_context_tokens") {
                record.insert("maxContextTokens".into(), Value::from(tokens));
            }
            for (field, column) in [("checkedAt", "checked_at"), ("warning", "warning")] {
                if let Some(text) = Row::text(&row, column) {
                    record.insert(field.into(), Value::String(text));
                }
            }
            let record = Value::Object(record);
            if !capabilities(&record) {
                self.ignore(format!(
                    "capabilities of {provider_id}: the record is malformed"
                ))?;
                continue;
            }
            if !self.setting_exists(PROVIDERS_NAMESPACE, &provider_id)? {
                self.ignore(format!(
                    "capabilities of {provider_id}: its provider was not imported"
                ))?;
                continue;
            }
            self.setting(PROVIDER_CAPABILITIES_NAMESPACE, &provider_id, &record)?;
        }
        Ok(())
    }

    /// Whether one compatibility record is already in the destination.
    fn setting_exists(&self, namespace: &str, key: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE namespace=? AND key=?)",
            params![namespace, key],
            |row| row.get(0),
        )?)
    }
}

impl<'a> Importer<'a> {
    /// `mcp_servers` has no table in the Rust store, so a config is preserved
    /// whole as the record the TypeScript store's `recordFromRow` would have
    /// handed back — one settings row per server, under its own namespace.
    fn import_mcp_servers(&mut self) -> Result<()> {
        if !self.table_exists("mcp_servers")? {
            return Ok(());
        }
        let mut servers = Vec::new();
        for row in self.rows("mcp_servers")? {
            let Some(id) = Row::text(&row, "id") else {
                self.report
                    .warnings
                    .push("mcp_servers: a row has no id".into());
                continue;
            };
            let Some(alias) = Row::text(&row, "alias") else {
                self.ignore(format!("mcp server {id}: alias is not there"))?;
                continue;
            };
            let Some(transport) = self.json_column("mcp server", &id, &row, "transport")? else {
                continue;
            };
            if !transport.is_object() {
                self.ignore(format!("mcp server {id}: transport is not an object"))?;
                continue;
            }
            let mut record = Map::new();
            record.insert("id".into(), Value::String(id.clone()));
            record.insert("alias".into(), Value::String(alias));
            record.insert("transport".into(), transport);
            for (field, column) in [
                ("secretRefs", "secret_refs"),
                ("toolAliases", "tool_aliases"),
                ("resilience", "resilience"),
            ] {
                if let Some(value) = self.optional_json("mcp server", &id, &row, column)? {
                    if value.is_object() {
                        record.insert(field.into(), value);
                    } else {
                        self.report.warnings.push(format!(
                            "mcp server {id}: {column} is not an object; the field was dropped"
                        ));
                    }
                }
            }
            // SQL NULL is an ABSENT `enabled`, which the port reads as "default
            // true"; collapsing the two would rewrite the config.
            if let Some(enabled) = Row::flag(&row, "enabled") {
                record.insert("enabled".into(), Value::Bool(enabled));
            }
            for (field, column) in [("createdAt", "created_at"), ("updatedAt", "updated_at")] {
                let Some(text) = Row::text(&row, column) else {
                    self.ignore(format!("mcp server {id}: {column} is not there"))?;
                    break;
                };
                record.insert(field.into(), Value::String(text));
            }
            if record.len() < 5 {
                continue;
            }
            let value = Value::Object(record);
            if self.setting(MCP_SERVERS_NAMESPACE, &id, &value)? {
                self.report.mcp_servers += 1;
                servers.push(value);
            }
        }
        if let Some(application_id) = self.opts.promote_mcp_application_id.clone() {
            self.promote_mcp_servers(&application_id, servers)?;
        }
        Ok(())
    }

    /// Aggregate the imported per-server compatibility records into the ONE
    /// `McpConfigRepository` document the production reader consults
    /// (namespace `mcp-config`, key = application id). `McpConfigRepository`
    /// reads a JSON array; per-key rows are invisible to it, so without this
    /// promotion an imported deployment starts with zero MCP servers while
    /// its storage shows them "copied". A live non-empty document is never
    /// overwritten; a server the production validator (`parse_record`: the
    /// `McpServerDto` shape plus the alias pattern) could not accept is
    /// excluded with a warning instead of poisoning the live document.
    fn promote_mcp_servers(&mut self, application_id: &str, servers: Vec<Value>) -> Result<()> {
        if application_id.is_empty() {
            self.report.warnings.push(
                "mcp-config: the promotion application id is empty; production \
                 promotion was skipped"
                    .into(),
            );
            return Ok(());
        }
        let mut accepted = Vec::new();
        for server in servers {
            match production_mcp_record(&server) {
                Ok(shaped) => accepted.push(shaped),
                Err(reason) => self.report.warnings.push(format!(
                    "mcp server {}: excluded from the production mcp-config document \
                     ({reason}); it remains under the compatibility namespace",
                    server["id"].as_str().unwrap_or_default()
                )),
            }
        }
        if accepted.is_empty() {
            return Ok(());
        }
        accepted.sort_by(|left, right| {
            (
                left["createdAt"].as_str().unwrap_or_default(),
                left["id"].as_str().unwrap_or_default(),
            )
                .cmp(&(
                    right["createdAt"].as_str().unwrap_or_default(),
                    right["id"].as_str().unwrap_or_default(),
                ))
        });
        let count = accepted.len();
        let document = Value::Array(accepted);
        let promoted = match self.setting_value(MCP_CONFIG_NAMESPACE, application_id)? {
            Some(existing) => {
                if existing.as_array().is_some_and(|items| !items.is_empty()) {
                    self.report.warnings.push(format!(
                        "mcp-config/{application_id}: a live configuration document \
                         already exists; imported servers were kept only under the \
                         compatibility namespace"
                    ));
                    false
                } else {
                    // An empty or malformed document carries nothing to lose;
                    // replace it with the imported servers.
                    self.setting(MCP_CONFIG_NAMESPACE, application_id, &document)?
                }
            }
            None => self.setting_if_absent(MCP_CONFIG_NAMESPACE, application_id, &document)?,
        };
        if promoted {
            self.report.promoted_mcp_servers += count;
        }
        Ok(())
    }

    /// The assistant-wide single-row `settings` table, preserved as
    /// `AssistantSettings` JSON under [`ASSISTANT_SETTINGS_NAMESPACE`].
    fn import_settings(&mut self) -> Result<()> {
        if !self.table_exists("settings")? {
            return Ok(());
        }
        for row in self.rows("settings")? {
            let mut record = Map::new();
            for (field, column) in [
                ("defaultProviderId", "default_provider_id"),
                ("defaultModel", "default_model"),
            ] {
                if let Some(text) = Row::text(&row, column) {
                    record.insert(field.into(), Value::String(text));
                }
            }
            for (field, column) in [
                ("contextSizePreference", "context_size_preference"),
                ("writePolicyMode", "write_policy_mode"),
                ("toolCalling", "tool_calling_mode"),
            ] {
                let Some(text) = Row::text(&row, column) else {
                    self.ignore(format!("assistant settings: {column} is not there"))?;
                    break;
                };
                record.insert(field.into(), Value::String(text));
            }
            if record.len() < 3 {
                continue;
            }
            let Some(allow_raw_tool_data) = Row::flag(&row, "allow_raw_tool_data") else {
                self.ignore("assistant settings: allow_raw_tool_data is not a flag")?;
                continue;
            };
            record.insert("allowRawToolData".into(), Value::Bool(allow_raw_tool_data));
            if let Some(iterations) = Row::integer(&row, "max_tool_iterations") {
                record.insert("maxToolIterations".into(), Value::from(iterations));
            }
            let metadata = self
                .optional_json("assistant settings", "settings", &row, "metadata")?
                .filter(Value::is_object)
                .unwrap_or_else(|| Value::Object(Map::new()));
            record.insert("metadata".into(), metadata);
            if self.setting(
                ASSISTANT_SETTINGS_NAMESPACE,
                ASSISTANT_SETTINGS_KEY,
                &Value::Object(record.clone()),
            )? && self.opts.promote_settings
            {
                // The runtime policy gate (`ChatExecutor::stage_tools`) and
                // the turn lane read ONLY the production document; a settings
                // row that lives just in the compatibility namespace has no
                // effect on any executor. Copy the wire-identical whitelist
                // fields and insert-if-absent, so an already-configured
                // deployment keeps its own document.
                let mut document = Map::new();
                for field in [
                    "defaultProviderId",
                    "defaultModel",
                    "contextSizePreference",
                    "writePolicyMode",
                    "allowRawToolData",
                    "maxToolIterations",
                    "toolCalling",
                    "metadata",
                ] {
                    if let Some(value) = record.get(field) {
                        document.insert(field.to_owned(), value.clone());
                    }
                }
                if self.setting_if_absent(
                    SETTINGS_NAMESPACE,
                    SETTINGS_KEY,
                    &Value::Object(document),
                )? {
                    self.report.promoted_settings += 1;
                } else {
                    self.report.warnings.push(
                        "assistant settings: the production settings document already \
                         exists; the imported document was kept only under the \
                         compatibility namespace and the live document still wins"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// `agent_inputs` is the same record on both sides — ADR 0016 §2 kept the
    /// field names — so a row becomes its own [`AgentInputRecord`] JSON plus
    /// the four columns the Rust table indexes. The shared `input_order`
    /// counter is then pushed past every imported order, so an input created
    /// after the import can never claim an imported position.
    fn import_inputs(&mut self) -> Result<()> {
        if !self.table_exists("agent_inputs")? {
            return Ok(());
        }
        let mut highest = 0_i64;
        for row in self.rows("agent_inputs")? {
            let Some(id) = Row::text(&row, "input_id") else {
                self.report
                    .warnings
                    .push("agent_inputs: a row has no id".into());
                continue;
            };
            let mut record = Map::new();
            record.insert("inputId".into(), Value::String(id.clone()));
            let mut missing = false;
            for (field, column) in [
                ("sessionId", "session_id"),
                ("kind", "kind"),
                ("status", "status"),
                ("contentHash", "content_hash"),
                ("createdAt", "created_at"),
                ("updatedAt", "updated_at"),
            ] {
                let Some(text) = Row::text(&row, column) else {
                    self.ignore(format!("agent input {id}: {column} is not there"))?;
                    missing = true;
                    break;
                };
                record.insert(field.into(), Value::String(text));
            }
            if missing {
                continue;
            }
            let Some(content) = self.json_column("agent input", &id, &row, "content")? else {
                continue;
            };
            record.insert("content".into(), content);
            let Some(order) = Row::integer(&row, "input_order") else {
                self.ignore(format!("agent input {id}: input_order is not a number"))?;
                continue;
            };
            record.insert("order".into(), Value::from(order));
            for (field, column) in [
                ("targetTaskId", "target_task_id"),
                ("onSettled", "on_settled"),
                ("attemptId", "attempt_id"),
                ("toolCallId", "tool_call_id"),
                ("expiresAt", "expires_at"),
                ("appliedTaskId", "applied_task_id"),
                ("appliedMessageId", "applied_message_id"),
                ("answerId", "answer_id"),
            ] {
                if let Some(text) = Row::text(&row, column) {
                    record.insert(field.into(), Value::String(text));
                }
            }
            // The destination reads this column back as the typed record, so a
            // kind, status or settle policy outside the state machine is
            // refused here rather than poisoning a later read.
            let Ok(parsed) =
                serde_json::from_value::<AgentInputRecord>(Value::Object(record.clone()))
            else {
                self.ignore(format!(
                    "agent input {id}: kind, status or on_settled is outside the machine"
                ))?;
                continue;
            };
            highest = highest.max(parsed.order);
            let text = json(self.db, &Value::Object(record))?;
            let (session, kind, status, target) = (
                parsed.session_id.clone(),
                crate::agent_inputs::kind_text(parsed.kind).to_owned(),
                crate::agent_inputs::status_text(parsed.status).to_owned(),
                parsed.target_task_id.clone(),
            );
            if self.attempt("agent input", &id, |db| {
                db.execute(
                    "INSERT INTO agent_inputs VALUES(?,?,?,?,?,?) ON CONFLICT(input_id) DO UPDATE \
                     SET session_id=excluded.session_id, kind=excluded.kind, \
                     status=excluded.status, target_task_id=excluded.target_task_id, \
                     record=excluded.record",
                    params![id, session, kind, status, target, text],
                )?;
                Ok(())
            })? {
                self.report.agent_inputs += 1;
            }
        }
        self.counter("input_order", highest)
    }

    /// `context_summaries` has no table in the Rust store; the Rust port keeps
    /// the identical record in the settings compatibility namespace, keyed by
    /// summary id. The destination's own validator gates every row.
    fn import_context_summaries(&mut self) -> Result<()> {
        if !self.table_exists("context_summaries")? {
            return Ok(());
        }
        for row in self.rows("context_summaries")? {
            let Some(id) = Row::text(&row, "summary_id") else {
                self.report
                    .warnings
                    .push("context_summaries: a row has no id".into());
                continue;
            };
            let mut record = Map::new();
            record.insert("summaryId".into(), Value::String(id.clone()));
            let mut missing = false;
            for (field, column) in [
                ("sessionId", "session_id"),
                ("coversThroughMessageId", "covers_through_message_id"),
                ("firstKeptMessageId", "first_kept_message_id"),
                ("summary", "summary"),
                ("sourceHash", "source_hash"),
                ("model", "model"),
                ("summarizerRevision", "summarizer_revision"),
                // The Rust record keeps an instant as the JavaScript string it
                // was written from; re-rendering it would rewrite history.
                ("createdAt", "created_at"),
            ] {
                let Some(text) = Row::text(&row, column) else {
                    self.ignore(format!("context summary {id}: {column} is not there"))?;
                    missing = true;
                    break;
                };
                record.insert(field.into(), Value::String(text));
            }
            if missing {
                continue;
            }
            for (field, column) in [
                ("tokensBefore", "tokens_before"),
                ("tokensAfter", "tokens_after"),
            ] {
                let Some(tokens) = Row::integer(&row, column) else {
                    self.ignore(format!("context summary {id}: {column} is not a number"))?;
                    missing = true;
                    break;
                };
                record.insert(field.into(), Value::from(tokens));
            }
            if missing {
                continue;
            }
            if let Some(provider_id) = Row::text(&row, "provider_id") {
                record.insert("providerId".into(), Value::String(provider_id));
            }
            let Some(details) = self.json_column("context summary", &id, &row, "details")? else {
                continue;
            };
            record.insert("details".into(), details);
            let record = Value::Object(record);
            if !crate::context_summaries::valid(&record) {
                self.ignore(format!(
                    "context summary {id}: the record is malformed (details must carry \
                     readFiles and modifiedFiles string lists)"
                ))?;
                continue;
            }
            if self.setting(CONTEXT_SUMMARIES_NAMESPACE, &id, &record)? {
                self.report.context_summaries += 1;
            }
        }
        Ok(())
    }

    /// Push one shared counter past everything the import carried.
    fn counter(&self, name: &str, highest: i64) -> Result<()> {
        if highest <= 0 || self.opts.dry_run {
            return Ok(());
        }
        self.db.execute(
            "UPDATE counters SET value=MAX(value,?) WHERE name=?",
            params![highest, name],
        )?;
        Ok(())
    }
}

impl<'a> Importer<'a> {
    /// Nothing is dropped in silence: a table the Rust store has no home for is
    /// named here, with how many rows it holds.
    fn report_unimported(&mut self) -> Result<()> {
        const UNREPRESENTED: &[(&str, &str)] = &[
            ("prompt_templates", "managed prompt templates"),
            ("connector_connections", "managed connector connections"),
        ];
        for (table, why) in UNREPRESENTED {
            if !self.table_exists(table)? {
                continue;
            }
            let rows: i64 =
                self.source
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
            if rows > 0 {
                self.report
                    .warnings
                    .push(format!("{table}: {rows} row(s) were not imported — {why}"));
            }
        }
        if self.table_exists("message_search")? {
            self.report.warnings.push(
                "message_search: the FTS5 index is derived state and was not copied; the Rust \
                 store answers search from the imported message records"
                    .into(),
            );
        }
        Ok(())
    }
}

/// The TypeScript layout is the contract: a core table or column that is not
/// there means this is not an adapters-sqlite assistant database.
fn require_table(source: &Connection, table: &str, columns: &[&str]) -> Result<()> {
    let present: bool = source.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |row| row.get(0),
    )?;
    if !present {
        return Err(StoreError::InvalidImport {
            reason: format!("the database has no {table} table; it is not an assistant store"),
        });
    }
    // `table` is a literal from this module.
    let mut statement = source.prepare(&format!("PRAGMA table_info({table})"))?;
    let present: HashSet<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for column in columns {
        if !present.contains(*column) {
            return Err(StoreError::InvalidImport {
                reason: format!(
                    "{table}.{column} is not there; the layout is not the TypeScript one"
                ),
            });
        }
    }
    Ok(())
}

/// The `McpServerDto` shape `McpConfigRepository` accepts, validated the way
/// its `parse_record` gate does: the nine carried keys, non-empty `id`,
/// `createdAt` and `updatedAt` strings, an object `transport`, string-map
/// optionals, a boolean `enabled`, and the alias pattern
/// `^[a-z][a-z0-9-]*$` that `create` enforces. Anything else is refused with
/// the reason, so a record that would be rejected on the next live commit
/// never enters the production document.
fn production_mcp_record(record: &Value) -> std::result::Result<Value, String> {
    let fields = record.as_object().ok_or("record is not an object")?;
    let text = |name: &str| -> std::result::Result<String, String> {
        fields
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("{name} is not a non-empty string"))
    };
    let id = text("id")?;
    let alias = text("alias")?;
    if !mcp_alias_allowed(&alias) {
        return Err(format!(
            "alias {alias:?} is outside the ^[a-z][a-z0-9-]*$ pattern the \
             production reader enforces"
        ));
    }
    if !fields.get("transport").is_some_and(Value::is_object) {
        return Err("transport is not an object".into());
    }
    let created_at = text("createdAt")?;
    let updated_at = text("updatedAt")?;
    let mut shaped = Map::new();
    shaped.insert("id".into(), Value::String(id));
    shaped.insert("alias".into(), Value::String(alias));
    shaped.insert("transport".into(), fields["transport"].clone());
    for name in ["secretRefs", "toolAliases", "resilience"] {
        match fields.get(name) {
            None | Some(Value::Null) => continue,
            Some(Value::Object(entries)) if entries.values().all(Value::is_string) => {
                shaped.insert(name.into(), fields[name].clone());
            }
            Some(_) => return Err(format!("{name} is not a string-to-string object")),
        }
    }
    match fields.get("enabled") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(enabled)) => {
            shaped.insert("enabled".into(), Value::Bool(*enabled));
        }
        Some(_) => return Err("enabled is not a boolean".into()),
    }
    shaped.insert("createdAt".into(), Value::String(created_at));
    shaped.insert("updatedAt".into(), Value::String(updated_at));
    Ok(Value::Object(shaped))
}

/// `^[a-z][a-z0-9-]*$` without pulling a regex dependency into the store.
fn mcp_alias_allowed(alias: &str) -> bool {
    let mut characters = alias.chars();
    matches!(characters.next(), Some(first) if first.is_ascii_lowercase())
        && characters
            .all(|character| character.is_ascii_lowercase() || matches!(character, '0'..='9' | '-'))
}

/// The least the envelope needs, rebuilt from the shredded columns when a
/// row's `payload` cannot speak for itself.
fn rebuilt_event(row: &Row, seq: f64, event_id: &str) -> Value {
    let mut event = Map::new();
    event.insert(
        "type".into(),
        Value::String(Row::text(row, "type").unwrap_or_default()),
    );
    event.insert("seq".into(), Value::from(seq));
    event.insert("eventId".into(), Value::String(event_id.to_owned()));
    if let Some(timestamp) = Row::text(row, "timestamp") {
        event.insert("timestamp".into(), Value::String(timestamp));
    }
    if let Some(attempt_id) = Row::text(row, "attempt_id") {
        event.insert("attemptId".into(), Value::String(attempt_id));
    }
    Value::Object(event)
}
