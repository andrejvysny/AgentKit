/** Incremental v8-to-v9 migration; the shipped v8 baseline stays immutable. */
export const SCHEMA_V9_PROVIDER_CONTINUATIONS = `
CREATE TABLE provider_run_scopes (
  run_id TEXT PRIMARY KEY REFERENCES tasks(task_id) ON DELETE CASCADE,
  scope_json TEXT NOT NULL,
  state_required INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE provider_continuations (
  run_id TEXT PRIMARY KEY REFERENCES tasks(task_id) ON DELETE CASCADE,
  anchor_message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
  state_json TEXT NOT NULL
);

CREATE INDEX idx_provider_continuations_anchor
  ON provider_continuations(chat_id, anchor_message_id);
`;
