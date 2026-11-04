/**
 * Fixture author-time generator for the imported-receipt acceptance tests
 * (`rust/crates/host/tests/imported_receipts.rs`).
 *
 * Run: `bun generate.ts` (from this directory). It writes the immutable
 * fixtures the tests read:
 *
 *   admitted.sqlite  — a TypeScript adapters-sqlite store (application_id 0,
 *                      user_version 10) with one chat, the run's message, a
 *                      failed `chat.turn` task, and ONE `admitted`-without-
 *                      result tool_executions receipt for `mutate` under the
 *                      integer-like/unicode-keyed ARGS below.
 *   completed.sqlite — the same layout plus a COMPLETED receipt carrying a
 *                      real ToolResult wire shape, declared unanswered by an
 *                      assistant message (the host replays it).
 *   manifest.json    — the arguments, the expected digests (TS pipeline),
 *                      canonical-JSON key-order samples, and the recorded
 *                      result. The tests embed it via include_str!.
 *
 * The digests come from the ACTUAL TypeScript hash implementation
 * (`packages/host/src/sessions/hash.ts`): `argsSha256Of` → `sha256Hex` of
 * `canonicalJson`, so the fixtures pin the cross-language contract, not a
 * Rust-side re-derivation.
 */
import { Database } from "bun:sqlite";
import { rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import {
  argsSha256Of,
  canonicalJson,
  sha256Hex,
} from "../../../../../../packages/host/src/sessions/hash.ts";

const here = dirname(import.meta.path);

const T0 = "2026-01-02T03:04:05.678Z";
const CHAT = "chat-1";
const RUN = "run-ts";

// The arguments an imported receipt binds and the live test reissues:
// integer-like keys (out of lexical order), the 2^32-1 index boundary,
// leading-zero / negative spellings that are NOT indices, non-BMP vs
// BMP unicode (UTF-16 code-unit order differs from UTF-8 byte order),
// and JSON number spellings JavaScript normalizes (1.0 → 1, 1e21, 1e-7, -0).
const ARGS = {
  "10": "a",
  "2": "b",
  "4294967294": "index-max",
  "4294967295": "over-index",
  "01": "not-index",
  "-1": "negative",
  text: "write this",
  nested: {
    "2": 2,
    "10": 10,
    é: "e-acute",
    z: "z",
    "\u{1F600}": "emoji",
    "\uFFFE": "fffe",
  },
  counts: { n: 1.0, big: 1e21, small: 1e-7, neg0: -0 },
  path: 'dir/file"x\nline\u0001',
};

// The host execution-context binding the receipt was admitted under. All
// plain strings/objects so both ports serialize it identically; the host
// test recomputes the native hash and asserts agreement.
const CONTEXT = {
  chatId: CHAT,
  scopeId: CHAT,
  bindings: [],
  toolName: "mutate",
  toolVersion: "1",
  context: {},
};

// The completed receipt's stored result: a REAL ToolResult wire shape
// (`packages/contracts/src/tool.ts` AiToolResultSchema — ok, data,
// sources, warnings, truncated, limits{profile,maxBytes,maxItems}).
const RECORDED_RESULT = {
  ok: true,
  data: { marker: "ts-replayed", executions: 7 },
  sources: [],
  warnings: [],
  truncated: false,
  limits: { profile: "small", maxBytes: 16384, maxItems: 50 },
  summary: "recorded under TypeScript",
};

// Canonical-JSON key-order samples pinned for the guard's TS-compatible
// canonicalizer: integer-like, boundary, and unicode orderings.
const SAMPLES: Array<Record<string, unknown>> = [
  { "10": "a", "2": "b" },
  {
    "4294967294": "max",
    "4294967295": "over",
    "-1": "neg",
    "01": "lz",
    "1.0": "f",
    "+1": "p",
    "0": "z",
  },
  { "\u{1F600}": "emoji", "\uFFFE": "fffe", é: "acute", z: "zed" },
  { arr: [{ "2": 1, b: 2, "10": 3 }], top: "x" },
  { empty: {}, list: [], t: true, n: null, s: 'q"\\\u0000\u001fdel\u007f' },
];

const DDL = `
CREATE TABLE chats (
  id TEXT PRIMARY KEY, title TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  metadata TEXT NOT NULL DEFAULT '{}', archived INTEGER NOT NULL DEFAULT 0);
CREATE TABLE messages (
  id TEXT PRIMARY KEY,
  chat_id TEXT NOT NULL REFERENCES chats(id),
  run_id TEXT, role TEXT NOT NULL,
  content TEXT NOT NULL, content_format TEXT NOT NULL DEFAULT 'text',
  order_key INTEGER NOT NULL, tool_call_id TEXT, tool_calls TEXT, model_result_json TEXT,
  parent_message_id TEXT REFERENCES messages(id),
  depth INTEGER NOT NULL DEFAULT 0, branch_index INTEGER NOT NULL DEFAULT 0,
  active INTEGER NOT NULL DEFAULT 1, metadata TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0,
  UNIQUE (chat_id, order_key));
CREATE TABLE tasks (
  task_id TEXT PRIMARY KEY, kind TEXT NOT NULL, scope_id TEXT NOT NULL, status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0, enqueued_at TEXT NOT NULL, available_at TEXT NOT NULL,
  started_at TEXT, finished_at TEXT, payload TEXT NOT NULL, parent_task_id TEXT,
  depends_on TEXT, progress TEXT, error TEXT,
  attempt_count INTEGER NOT NULL DEFAULT 0, poison_count INTEGER NOT NULL DEFAULT 0,
  dead_lettered_at TEXT, dead_letter_reason TEXT);
CREATE TABLE task_attempts (
  attempt_id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(task_id),
  attempt_number INTEGER NOT NULL, status TEXT NOT NULL, owner_id TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, error TEXT);
CREATE TABLE leases (
  task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
  attempt_id TEXT NOT NULL, owner_id TEXT NOT NULL, lease_token TEXT NOT NULL UNIQUE,
  fencing_token INTEGER NOT NULL, expires_at TEXT NOT NULL);
CREATE TABLE task_events (
  task_id TEXT NOT NULL, seq INTEGER NOT NULL, event_id TEXT NOT NULL UNIQUE,
  attempt_id TEXT, type TEXT NOT NULL, timestamp TEXT NOT NULL, payload TEXT NOT NULL,
  PRIMARY KEY (task_id, seq));
CREATE TABLE tool_executions (
  execution_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
  approval_request_id TEXT UNIQUE, record TEXT NOT NULL);
`;

function build(
  path: string,
  completed: boolean,
  argsSha256: string,
  contextSha256: string,
) {
  rmSync(path, { force: true });
  const db = new Database(path);
  db.exec(DDL);
  db.exec("PRAGMA user_version = 10;");
  db.run("INSERT INTO chats(id,title,created_at,updated_at) VALUES(?,?,?,?)", [
    CHAT,
    "imported",
    T0,
    T0,
  ]);
  db.run(
    `INSERT INTO messages(id,chat_id,run_id,role,content,order_key,created_at)
     VALUES(?,?,?,?,?,?,?)`,
    ["m-user", CHAT, RUN, "user", "seed", 1, T0],
  );
  if (completed) {
    db.run(
      `INSERT INTO messages(id,chat_id,run_id,role,content,order_key,tool_calls,created_at)
       VALUES(?,?,?,?,?,?,?,?)`,
      [
        "m-asst",
        CHAT,
        RUN,
        "assistant",
        "",
        2,
        JSON.stringify([
          {
            id: "call-done",
            name: "mutate",
            argumentsJson: JSON.stringify(ARGS),
          },
        ]),
        T0,
      ],
    );
  }
  db.run(
    `INSERT INTO tasks(task_id,kind,scope_id,status,enqueued_at,available_at,payload)
     VALUES(?,?,?,?,?,?,?)`,
    [RUN, "chat.turn", CHAT, "failed", T0, T0, "{}"],
  );
  const record: Record<string, unknown> = {
    executionId: completed ? "exec-ts-done" : "exec-ts-uncertain",
    runId: RUN,
    toolCallId: completed ? "call-done" : "call-seed",
    toolName: "mutate",
    argsSha256,
    contextSha256,
    attemptId: "attempt-1",
    status: completed ? "completed" : "admitted",
  };
  if (completed) record.result = RECORDED_RESULT;
  db.run(
    "INSERT INTO tool_executions(execution_id,run_id,record) VALUES(?,?,?)",
    [record.executionId as string, RUN, JSON.stringify(record)],
  );
  db.close();
}

const argsSha256 = await argsSha256Of(ARGS);
const contextSha256 = await argsSha256Of(CONTEXT);
build(join(here, "admitted.sqlite"), false, argsSha256, contextSha256);
build(join(here, "completed.sqlite"), true, argsSha256, contextSha256);

const samples = [];
for (const input of SAMPLES) {
  const canonical = canonicalJson(input);
  samples.push({ input, canonical, sha256: await sha256Hex(canonical) });
}
writeFileSync(
  join(here, "manifest.json"),
  `${JSON.stringify(
    {
      note: "Generated by generate.ts through packages/host/src/sessions/hash.ts — do not hand-edit.",
      args: ARGS,
      argsCanonical: canonicalJson(ARGS),
      argsSha256Ts: argsSha256,
      contextCanonical: canonicalJson(CONTEXT),
      contextSha256Ts: contextSha256,
      recordedResult: RECORDED_RESULT,
      samples,
    },
    null,
    2,
  )}\n`,
);
console.log("fixtures written:", { argsSha256, contextSha256 });
