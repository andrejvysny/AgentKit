/**
 * v3 SQLite schema for {@link SqliteAssistantStore} — a single-file DDL string,
 * applied idempotently (every DDL statement is `CREATE ... IF NOT EXISTS`;
 * seed rows use `INSERT OR IGNORE`) so opening the same database twice, or
 * opening a database another process already initialized, is a no-op rather
 * than an error.
 *
 * Table-by-table home in the port model:
 * - `chats` / `messages` → `ConversationStore`
 * - `tasks` / `task_attempts` / `leases` / `task_events` → `TaskStore`
 * - `proposals` / `proposal_outcomes` → `ProposalStore`
 * - `providers` / `provider_models` / `provider_capabilities` → `ProviderStore`
 * - `settings` → `SettingsStore` (single row, id = 1)
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
export const SCHEMA_VERSION = 3;

/**
 * The DDL for {@link SCHEMA_VERSION}. There are NO migrations in this
 * workspace-private adapter: {@link SqliteAssistantStore} stamps
 * `PRAGMA user_version` on a fresh database and refuses to open one written by
 * a different version, because a reference adapter that shipped half-tested
 * migration scripts would be claiming a durability guarantee it does not have.
 * A host that needs upgrades in place owns that story with its own store.
 */
export const SCHEMA_V3 = `
CREATE TABLE IF NOT EXISTS chats (
  id TEXT PRIMARY KEY,
  title TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  metadata TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS messages (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL REFERENCES chats(id),
  run_id TEXT,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  order_key INTEGER NOT NULL,
  tool_call_id TEXT,
  tool_calls TEXT,
  model_result_json TEXT,
  metadata TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  UNIQUE (chat_id, order_key)
);
CREATE INDEX IF NOT EXISTS idx_messages_chat_order ON messages(chat_id, order_key);

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
  metadata TEXT NOT NULL DEFAULT '{}'
);
INSERT OR IGNORE INTO settings
  (id, context_size_preference, write_policy_mode, allow_raw_tool_data, metadata)
  VALUES (1, 'small', 'auto_readonly_confirm_writes', 0, '{}');

-- Backs TaskStore.acquireLease's store-global monotonic fencing token. Not a
-- port record — see the module doc comment above.
CREATE TABLE IF NOT EXISTS fencing_counter (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  value INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO fencing_counter (id, value) VALUES (1, 0);
`;
