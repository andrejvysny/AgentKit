/**
 * v8 SQLite schema for {@link SqliteAssistantStore} — a single-file DDL string,
 * applied idempotently (every DDL statement is `CREATE ... IF NOT EXISTS`;
 * seed rows use `INSERT OR IGNORE`) so opening the same database twice, or
 * opening a database another process already initialized, is a no-op rather
 * than an error.
 *
 * Table-by-table home in the port model:
 * - `chats` / `messages` → `ConversationStore`
 * - `message_search_source` (a VIEW) / `message_search` (FTS5) → also
 *   `ConversationStore`, backing `searchMessages`. Not port records: an index
 *   is a derived view of `messages`, kept in step by triggers, and nothing
 *   outside this file reads either name.
 * - `tasks` / `task_attempts` / `leases` / `task_events` → `TaskStore`
 * - `proposals` / `proposal_outcomes` → `ProposalStore`
 * - `providers` / `provider_models` / `provider_capabilities` → `ProviderStore`
 * - `settings` → `SettingsStore` (single row, id = 1)
 * - `mcp_servers` → `McpServerConfigStore` (`@agentkit/mcp-client`), served by
 *   {@link SqliteMcpServerConfigStore} — a STANDALONE store over this same
 *   database, not a member of the `AssistantStore` aggregate. The table lives
 *   here anyway because the file is one database with one `user_version`, and a
 *   second DDL string applied by a second constructor is a second thing that
 *   can be forgotten.
 * - `outbox` → `OutboxStore`
 * - `fencing_counter` → NOT a port record. A single-row table backing the
 *   store-global monotonic fencing token `TaskStore.acquireLease` hands out;
 *   it has no equivalent in any port because fencing is an implementation
 *   detail of lease issuance, never something a caller reads directly.
 *
 * Queue state lives on `tasks` + `leases`; there is no separate queue table —
 * `claimNext` computes effective priority (base priority plus the aging term,
 * off unless the store was constructed with one) in the query itself, so there
 * is nothing to keep in sync or let drift. Dependency edges live on `tasks`
 * too, as a JSON `depends_on` array rather than an edge table: they are
 * immutable after create and only ever read for the one task being gated, so a
 * join table would buy nothing and cost a second write per submit.
 *
 * JSON-shaped fields (`payload`, `metadata`, `envelope`, `operations`,
 * `warnings`, `tool_calls`, `failed_ops`, `extra_headers`, `decision`, ...) are
 * stored as TEXT; the store (de)serializes them, SQLite never inspects their
 * contents.
 */
export const SCHEMA_VERSION = 8;

/**
 * The text {@link SCHEMA_V8}'s search index sees for one message row, as SQL.
 *
 * The definition is `searchTextOf` in `@agentkit/host` — a string body as
 * itself, a parts body as ALL of its text parts joined by a newline — expressed
 * three times over three different row aliases (`m` in the view, `new` and
 * `old` in the triggers) because SQLite has no way to share it. Generated from
 * one function so the three cannot drift: an index and a delete-trigger that
 * disagreed about a message's text would leave the index quietly wrong,
 * detectable only by searching for something that used to be there.
 *
 * ALL text parts, not the first. Indexing only part one is a real bug in the
 * system this design is copied from, and it fails silently — the search box
 * simply never finds a phrase that happened to land in paragraph two.
 * Non-text parts contribute nothing: base64 image bytes are not prose.
 */
function messageSearchText(alias: string): string {
  return `CASE WHEN ${alias}.content_format = 'parts'
      THEN COALESCE(
        (SELECT group_concat(json_extract(part.value, '$.text'), char(10))
           FROM json_each(${alias}.content) AS part
          WHERE json_extract(part.value, '$.type') = 'text'),
        '')
      ELSE ${alias}.content
    END`;
}

/**
 * The DDL for {@link SCHEMA_VERSION}. There are NO migrations in this
 * workspace-private adapter: {@link SqliteAssistantStore} stamps
 * `PRAGMA user_version` on a fresh database and refuses to open one written by
 * a different version, because a reference adapter that shipped half-tested
 * migration scripts would be claiming a durability guarantee it does not have.
 * A host that needs upgrades in place owns that story with its own store.
 *

 * That refusal IS the v7 → v8 upgrade path, exactly as it was the v6 → v7 and
 * v5 → v6 ones: a database stamped 7 raises `sqlite_schema_version` and is
 * recreated. The `DEFAULT` clauses on the newer columns (`chats.archived`,
 * `messages.content_format`, `settings.tool_calling_mode`) are therefore not
 * migration aids — they are what keeps the DDL re-appliable over a database
 * this build already wrote, which is the property every statement here has.
 *
 * v8 adds two things, both durability follow-ups rather than features:
 * `idx_messages_run` on `messages(chat_id, run_id, depth, order_key)`, which is
 * `lastMessageOfRun`'s whole query — the lookup that opens EVERY turn attempt
 * by linking it to its own chain, and until now a scan of one chat's whole
 * message table each time — and `proposals.claimed_at`, the instant an apply
 * took the `approved → applying` claim. The reconcile window keys on that column: the
 * stamps it used before (`applied_at`, `decided_at`, `created_at`) are all
 * OLDER than the claim for a write a human approved and something applied
 * later, so a live apply could look stale and be reconciled out from under
 * itself. NULLABLE, because every row written before v8 has no claim instant
 * to record and the fallback chain still answers for them.
 *
 * v7 adds one table: `mcp_servers`, the durable half of
 * `McpServerConfigStore`. It carries NO secret material — `secret_refs` holds
 * {@link SecretStore} REFS, and the values behind them are injected into
 * env/header placeholders at connect time and stored nowhere.
 *
 * v6 added three things: `chats.archived` (the listing filter),
 * `settings.tool_calling_mode` (the manual tool-calling override), and the FTS5
 * machinery behind `ConversationStore.searchMessages` — a view computing the
 * searchable text of every message, an EXTERNAL-CONTENT FTS5 table over that
 * view, three triggers keeping the two in step, and a guarded backfill.
 *
 * External content, rather than a plain FTS5 table, because a plain one stores
 * a SECOND COPY of every message body: the index would double the size of the
 * only table that holds user text, to serve a feature most rows are never
 * searched for. Pointing FTS5 at a view costs nothing at rest and re-computes
 * the projection only when `snippet()` needs it.
 */
export const SCHEMA_V8 = `
CREATE TABLE IF NOT EXISTS chats (
  id TEXT PRIMARY KEY,
  title TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  metadata TEXT NOT NULL DEFAULT '{}',
  -- Hidden from the default listing; a real column rather than a metadata key
  -- because listChats filters on it and an index cannot reach into a JSON bag.
  archived INTEGER NOT NULL DEFAULT 0
);
-- listChats' default query: unarchived chats, newest first.
CREATE INDEX IF NOT EXISTS idx_chats_archived_updated ON chats(archived, updated_at DESC);

-- A chat's messages are a TREE (parent_message_id is a self-reference), and
-- the active column is the per-message flag marking which root-to-leaf path
-- through it the conversation currently is. A flag, rather than a pointer to the
-- live leaf, is what makes "read the conversation" one indexed range scan
-- instead of a recursive walk. depth is denormalized for exactly that scan: it
-- is derivable from the parent chain, but deriving it is the walk being avoided.
--
-- The self-FK is real (foreign_keys is ON), so a message can never name a parent
-- that is not there -- including across a fork, whose copies are inserted parent
-- first inside one transaction.
CREATE TABLE IF NOT EXISTS messages (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL REFERENCES chats(id),
  run_id TEXT,
  role TEXT NOT NULL,
  -- content holds the message body; content_format says how to read it.
  -- 'text' -- the column IS the string, byte for byte, exactly as every row
  -- written before v5 was. 'parts' -- the column is a JSON array of
  -- AiContentPart (@agentkit/contracts), serialized by the store and never
  -- inspected by SQLite.
  --
  -- A format column rather than "try JSON.parse and fall back": a user message
  -- whose text happens to be a JSON array of part-shaped objects is a STRING,
  -- and a store that guessed would silently promote it to parts on the next
  -- read. One byte of bookkeeping removes the ambiguity permanently.
  --
  -- B2 (full-text search) indexes the text of a message: for 'text' rows that is
  -- this column, for 'parts' rows it is the concatenated text parts, and the
  -- trigger/virtual-table pair that maintains it hangs off this table.
  content TEXT NOT NULL,
  content_format TEXT NOT NULL DEFAULT 'text',
  order_key INTEGER NOT NULL,
  tool_call_id TEXT,
  tool_calls TEXT,
  model_result_json TEXT,
  parent_message_id TEXT REFERENCES messages(id),
  depth INTEGER NOT NULL DEFAULT 0,
  branch_index INTEGER NOT NULL DEFAULT 0,
  active INTEGER NOT NULL DEFAULT 1,
  metadata TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  UNIQUE (chat_id, order_key)
);
CREATE INDEX IF NOT EXISTS idx_messages_chat_order ON messages(chat_id, order_key);
-- listMessages' whole query: the active path of one chat, in path order.
CREATE INDEX IF NOT EXISTS idx_messages_active ON messages(chat_id, active, depth);
-- Sibling lookups, and the max(branch_index) an append reads to place itself.
CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_message_id, branch_index);
-- lastMessageOfRun's whole query: one run's deepest message in one chat, which
-- without this index scans every message of the chat on every turn resume.
-- depth is the third column so the ORDER BY reads it off the index backwards;
-- SQLite is left sorting only the LAST term (order_key), over the rows of one
-- run sitting at one depth.
CREATE INDEX IF NOT EXISTS idx_messages_run ON messages(chat_id, run_id, depth, order_key);

-- ── FULL-TEXT SEARCH ───────────────────────────────────────────────────────
-- The searchable projection of every message, as a view: string bodies as
-- themselves, parts bodies as ALL of their text parts joined by a newline.
CREATE VIEW IF NOT EXISTS message_search_source AS
SELECT m.rowid AS rowid, ${messageSearchText("m")} AS body FROM messages AS m;

-- External content (content=<the view above>): the index holds terms and
-- postings, never a second copy of the text. snippet() re-reads the body
-- through the view when a hit needs one.
--
-- THE INDEX IS KEYED BY messages.rowid, SO DO NOT VACUUM A LIVE STORE FILE.
-- messages has a TEXT primary key, so its rowid is SQLite's own, and SQLite
-- documents that VACUUM MAY RENUMBER the rowids of a table with no INTEGER
-- PRIMARY KEY (lang_vacuum.html). A renumbering rewrites the content table
-- without telling FTS5, so every posting silently starts naming a different
-- message -- wrong hits and snippets cut from bodies that never held the term,
-- with nothing that raises and no later write that repairs it. Rebuilding means
-- recreating the database, or re-running the backfill at the bottom of this
-- block by hand. See the VACUUM caveat in this package's README.
--
-- unicode61 with diacritic folding is the tokenizer a chat search wants: it
-- splits on punctuation and case, and makes "resume" find "résumé". No stemmer
-- -- an English-only stemmer applied to whatever language a user's chat happens
-- to be in makes matches worse, not better.
CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(
  body,
  content='message_search_source',
  content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);

-- Three triggers, because with external content NOTHING maintains the index on
-- its own: FTS5 reads the view only to fetch a body it was already told about.
-- A 'delete' command must carry the OLD text so FTS5 can remove exactly the
-- postings it inserted -- which is why the projection is repeated here rather
-- than re-read from the view, whose row is already gone by then.
CREATE TRIGGER IF NOT EXISTS messages_search_insert AFTER INSERT ON messages BEGIN
  INSERT INTO message_search(rowid, body) VALUES (new.rowid, ${messageSearchText("new")});
END;
CREATE TRIGGER IF NOT EXISTS messages_search_delete AFTER DELETE ON messages BEGIN
  INSERT INTO message_search(message_search, rowid, body)
    VALUES ('delete', old.rowid, ${messageSearchText("old")});
END;
-- updateMessage rewrites a streaming answer on every chunk, so this pair runs
-- often; it is still the cheapest correct thing, because FTS5 has no in-place
-- update for an external-content row.
CREATE TRIGGER IF NOT EXISTS messages_search_update AFTER UPDATE ON messages BEGIN
  INSERT INTO message_search(message_search, rowid, body)
    VALUES ('delete', old.rowid, ${messageSearchText("old")});
  INSERT INTO message_search(rowid, body) VALUES (new.rowid, ${messageSearchText("new")});
END;

-- Backfill, guarded so re-applying this DDL cannot double-index anything.
--
-- The guard reads the FTS5 shadow table rather than message_search itself, and
-- that is not squeamishness about internals: scanning an EXTERNAL-CONTENT fts5
-- table without a MATCH iterates the CONTENT SOURCE, so
-- SELECT 1 FROM message_search is non-empty whenever messages is -- the guard
-- would read "already populated" on a completely empty index and skip the
-- backfill it exists to perform. The %_docsize shadow table holds one row per
-- INDEXED document, which is the question actually being asked.
--
-- On a fresh database this inserts nothing (there are no messages yet); it
-- earns its place when an index is rebuilt from a store whose rows predate it.
INSERT INTO message_search(rowid, body)
  SELECT source.rowid, source.body FROM message_search_source AS source
   WHERE NOT EXISTS (SELECT 1 FROM message_search_docsize);

-- kind is what the executor registry dispatches on; there is no chat_id
-- column, because a task of an arbitrary kind has no conversation. Whatever a
-- kind needs (a chat turn's chatId included) rides in the payload column.
CREATE TABLE IF NOT EXISTS tasks (
  task_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  enqueued_at TEXT NOT NULL,
  available_at TEXT NOT NULL,
  started_at TEXT,
  finished_at TEXT,
  payload TEXT NOT NULL,
  -- Lineage (parent_task_id) and the claim gate (depends_on, a JSON array of
  -- task ids) are separate edges on purpose: a child runs independently of its
  -- parent, a dependent does not run until what it waits on completes. Neither
  -- is a foreign key — createTask proves both point at existing rows before it
  -- writes, and an FK would additionally block deleting an old completed task
  -- that some finished row still names.
  parent_task_id TEXT,
  depends_on TEXT,
  -- Last-known progress snapshot, overwritten by updateProgress. Never events.
  progress TEXT,
  error TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  poison_count INTEGER NOT NULL DEFAULT 0,
  dead_lettered_at TEXT,
  dead_letter_reason TEXT
);
-- The claim index stays (status, scope_id, available_at): those three are what
-- every claim filters on. kind is deliberately NOT in it — the kind filter is
-- an optional IN(...) that most deployments never pass, and a wider index would
-- cost every write to serve a predicate that usually is not there.
CREATE INDEX IF NOT EXISTS idx_tasks_claim ON tasks(status, scope_id, available_at);
-- listChildren's only query, and the cancel cascade walks it once per node.
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_task_id);

CREATE TABLE IF NOT EXISTS task_attempts (
  attempt_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(task_id),
  attempt_number INTEGER NOT NULL,
  status TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  started_at TEXT NOT NULL,
  ended_at TEXT,
  error TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_attempts_task ON task_attempts(task_id);

-- One row per task (PK task_id): acquireLease always mints a fresh lease and
-- replaces whatever was there, live or expired. See the module doc on
-- SqliteTaskStore.acquireLease for why that is safe.
CREATE TABLE IF NOT EXISTS leases (
  task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
  attempt_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  lease_token TEXT NOT NULL UNIQUE,
  fencing_token INTEGER NOT NULL,
  expires_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS task_events (
  task_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  attempt_id TEXT,
  type TEXT NOT NULL,
  timestamp TEXT NOT NULL,
  payload TEXT NOT NULL,
  PRIMARY KEY (task_id, seq)
);

CREATE TABLE IF NOT EXISTS outbox (
  id TEXT PRIMARY KEY,
  topic TEXT NOT NULL,
  run_id TEXT,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL,
  available_at TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  published_at TEXT,
  last_error TEXT
);
CREATE INDEX IF NOT EXISTS idx_outbox_claim ON outbox(published_at, available_at);

CREATE TABLE IF NOT EXISTS proposals (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL,
  run_id TEXT,
  scope_key TEXT NOT NULL,
  action_id TEXT,
  tool_name TEXT NOT NULL,
  kind TEXT NOT NULL,
  risk TEXT NOT NULL,
  status TEXT NOT NULL,
  envelope TEXT NOT NULL,
  operations TEXT NOT NULL,
  warnings TEXT NOT NULL,
  truncated INTEGER NOT NULL,
  revision_at_create TEXT,
  operation_id TEXT,
  decision TEXT,
  reason TEXT,
  created_at TEXT NOT NULL,
  decided_at TEXT,
  -- When the apply claim (approved -> applying) was taken. The reconcile
  -- window keys on it: decided_at is when a human said yes, which for a write
  -- applied much later is far older than the claim and would make a live apply
  -- look stuck. NULL on every row a pre-v8 build wrote and on every row that
  -- never reached the applying state.
  claimed_at TEXT,
  applied_at TEXT
);
-- The idempotency guarantee: a (scopeKey, actionId) pair is unique EXCEPT
-- among proposals that never wrote anything (ACTION_ID_RELEASING_STATUSES —
-- rejected/invalidated), whose key is free to reuse.
CREATE UNIQUE INDEX IF NOT EXISTS idx_proposals_action_id
  ON proposals(scope_key, action_id)
  WHERE action_id IS NOT NULL AND status NOT IN ('rejected', 'invalidated');
CREATE INDEX IF NOT EXISTS idx_proposals_chat ON proposals(chat_id, status);
CREATE INDEX IF NOT EXISTS idx_proposals_status ON proposals(status);

CREATE TABLE IF NOT EXISTS proposal_outcomes (
  operation_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  applied_ops INTEGER NOT NULL,
  failed_ops TEXT NOT NULL,
  result_json TEXT,
  revision TEXT
);

CREATE TABLE IF NOT EXISTS providers (
  id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  kind TEXT NOT NULL,
  base_url TEXT NOT NULL,
  api_key TEXT,
  default_model TEXT NOT NULL,
  enabled INTEGER NOT NULL,
  extra_headers TEXT,
  metadata TEXT
);

CREATE TABLE IF NOT EXISTS provider_models (
  provider_id TEXT NOT NULL REFERENCES providers(id),
  model_id TEXT NOT NULL,
  display_name TEXT,
  context_window_tokens INTEGER,
  supports_tool_calling INTEGER,
  fetched_at TEXT NOT NULL,
  PRIMARY KEY (provider_id, model_id)
);

CREATE TABLE IF NOT EXISTS provider_capabilities (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id),
  streaming INTEGER NOT NULL,
  tool_calling INTEGER NOT NULL,
  model_list INTEGER NOT NULL,
  vision INTEGER,
  json_mode INTEGER,
  max_context_tokens INTEGER,
  checked_at TEXT,
  warning TEXT
);

-- Single row (id = 1): assistant-wide settings, not per-chat state.
CREATE TABLE IF NOT EXISTS settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  default_provider_id TEXT,
  default_model TEXT,
  context_size_preference TEXT NOT NULL,
  write_policy_mode TEXT NOT NULL,
  allow_raw_tool_data INTEGER NOT NULL,
  max_tool_iterations INTEGER,
  -- 'auto' | 'on' | 'off'. Named _mode because provider_capabilities already
  -- has a BOOLEAN tool_calling (what was probed); this is what the operator
  -- decided on top of it.
  tool_calling_mode TEXT NOT NULL DEFAULT 'auto',
  metadata TEXT NOT NULL DEFAULT '{}'
);
INSERT OR IGNORE INTO settings
  (id, context_size_preference, write_policy_mode, allow_raw_tool_data, tool_calling_mode, metadata)
  VALUES (1, 'small', 'auto_readonly_confirm_writes', 0, 'auto', '{}');

-- McpServerConfigStore (@agentkit/mcp-client). Standalone: nothing in the
-- AssistantStore aggregate references it, and no other table references this
-- one -- an MCP server config shares a transaction with nothing.
--
-- NO SECRET MATERIAL. secret_refs is a JSON map of placeholder token ->
-- SecretStore REF; the values behind those refs are resolved at connect time
-- and never written here. Same rule the McpServerConfig record itself follows:
-- a config is listed, logged and shown in a UI.
CREATE TABLE IF NOT EXISTS mcp_servers (
  id TEXT PRIMARY KEY,
  -- The tool namespace, baked into every canonical id (mcp.<alias>.<tool>) a
  -- transcript records -- so two servers sharing one would mint the same id for
  -- two different tools. UNIQUE is the backstop; the store checks first so the
  -- caller gets a typed mcp_invalid_config rather than a driver error. BINARY
  -- collation (the default) makes it case-SENSITIVE, which is what the port
  -- specifies.
  alias TEXT NOT NULL UNIQUE,
  -- JSON McpTransportConfig: the stdio/http discriminated union, stored whole
  -- rather than shredded into columns. Its shape is the mcp-client package's to
  -- change, and a column per transport variant would make every new transport a
  -- schema version.
  transport TEXT NOT NULL,
  secret_refs TEXT,
  -- NULLABLE on purpose: McpServerConfig.enabled is OPTIONAL and absent means
  -- "default true". A NOT NULL DEFAULT 1 would round-trip an unset field as an
  -- explicit true, which is a different record from the one that was written.
  enabled INTEGER,
  tool_aliases TEXT,
  resilience TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
-- list() orders by createdAt; the id tie-breaks so two configs written in the
-- same millisecond still come back in a stable order.
CREATE INDEX IF NOT EXISTS idx_mcp_servers_created ON mcp_servers(created_at, id);

-- Backs TaskStore.acquireLease's store-global monotonic fencing token. Not a
-- port record — see the module doc comment above.
CREATE TABLE IF NOT EXISTS fencing_counter (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  value INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO fencing_counter (id, value) VALUES (1, 0);
`;
