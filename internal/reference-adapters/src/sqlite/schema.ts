/**
 * v1 SQLite schema for {@link SqliteAssistantStore} — a single-file DDL string,
 * applied idempotently (every DDL statement is `CREATE ... IF NOT EXISTS`;
 * seed rows use `INSERT OR IGNORE`) so opening the same database twice, or
 * opening a database another process already initialized, is a no-op rather
 * than an error.
 *
 * Table-by-table home in the port model:
 * - `chats` / `messages` → `ConversationStore`
 * - `runs` / `run_attempts` / `leases` / `run_events` → `RunStore`
 * - `proposals` / `proposal_outcomes` → `ProposalStore`
 * - `providers` / `provider_models` / `provider_capabilities` → `ProviderStore`
 * - `settings` → `SettingsStore` (single row, id = 1)
 * - `outbox` → `OutboxStore`
 * - `fencing_counter` → NOT a port record. A single-row table backing the
 *   store-global monotonic fencing token `RunStore.acquireLease` hands out;
 *   it has no equivalent in any port because fencing is an implementation
 *   detail of lease issuance, never something a caller reads directly.
 *
 * Queue state lives on `runs` + `leases`; there is no separate queue table —
 * `claimNext` computes effective priority (base priority + an age bucket) in
 * the query itself, so there is nothing to keep in sync or let drift.
 *
 * JSON-shaped fields (`request`, `metadata`, `envelope`, `operations`,
 * `warnings`, `tool_calls`, `payload`, `failed_ops`, `extra_headers`,
 * `decision`, ...) are stored as TEXT; the store (de)serializes them, SQLite
 * never inspects their contents.
 */
export const SCHEMA_V1 = `
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

CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  enqueued_at TEXT NOT NULL,
  available_at TEXT NOT NULL,
  started_at TEXT,
  finished_at TEXT,
  request TEXT NOT NULL,
  error TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  poison_count INTEGER NOT NULL DEFAULT 0,
  dead_lettered_at TEXT,
  dead_letter_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_claim ON runs(status, scope_id, available_at);

CREATE TABLE IF NOT EXISTS run_attempts (
  attempt_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(run_id),
  attempt_number INTEGER NOT NULL,
  status TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  started_at TEXT NOT NULL,
  ended_at TEXT,
  error TEXT
);
CREATE INDEX IF NOT EXISTS idx_run_attempts_run ON run_attempts(run_id);

-- One row per run (PK run_id): acquireLease always mints a fresh lease and
-- replaces whatever was there, live or expired. See the module doc on
-- SqliteRunStore.acquireLease for why that is safe.
CREATE TABLE IF NOT EXISTS leases (
  run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
  attempt_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  lease_token TEXT NOT NULL UNIQUE,
  fencing_token INTEGER NOT NULL,
  expires_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS run_events (
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  attempt_id TEXT,
  type TEXT NOT NULL,
  timestamp TEXT NOT NULL,
  payload TEXT NOT NULL,
  PRIMARY KEY (run_id, seq)
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

-- Backs RunStore.acquireLease's store-global monotonic fencing token. Not a
-- port record — see the module doc comment above.
CREATE TABLE IF NOT EXISTS fencing_counter (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  value INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO fencing_counter (id, value) VALUES (1, 0);
`;
