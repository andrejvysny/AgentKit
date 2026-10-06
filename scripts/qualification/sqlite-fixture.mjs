import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  SCHEMA_V8,
  SCHEMA_VERSION,
} from "../../packages/agentkit/dist/adapters-sqlite/schema.js";
import { SQLITE_MIGRATIONS } from "../../packages/agentkit/dist/adapters-sqlite/sqlite/migrations.js";
import { SharedSqliteAssistantStore } from "../../packages/agentkit/dist/adapters-sqlite/shared-assistant-store.js";
const bun = process.env.AGENTKIT_SQLITE_DRIVER === "bun";
const module = await import(
  `../../packages/agentkit/dist/adapters-sqlite/${bun ? "index" : "node"}.js`
);
const Store = bun
  ? module.SqliteAssistantStore
  : module.NodeSqliteAssistantStore;
const Raw = bun
  ? (await import("bun:sqlite")).Database
  : (
      await import(
        "../../packages/agentkit/dist/adapters-sqlite/sqlite/node-driver.js"
      )
    ).NodeSqliteDatabase;
function raw(path) {
  return bun ? new Raw(path, { safeIntegers: true }) : new Raw(path);
}

async function scratch(run) {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-durability-"));
  try {
    await run(join(dir, "assistant.db"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
async function seed(path, legacy = false) {
  let store;
  if (legacy) {
    const db = raw(path);
    db.exec(SCHEMA_V8);
    db.exec("PRAGMA user_version = 8; PRAGMA foreign_keys = ON");
    store = new SharedSqliteAssistantStore(db);
  } else store = new Store(path);
  try {
    await seedConversation(store);
    await seedTask(store);
    await seedProposal(store);
    await store.outbox.enqueue({
      id: "publication",
      topic: "task.events",
      runId: "task",
      payload: { durable: true },
    });
  } finally {
    store.close();
  }
}
async function seedConversation(store) {
  await store.conversations.createChat({ id: "chat" });
  await store.conversations.appendMessage({
    id: "message",
    chatId: "chat",
    role: "user",
    content: "persistent",
  });
  await store.conversations.appendMessage({
    id: "parts",
    chatId: "chat",
    role: "assistant",
    content: [
      { type: "text", text: "first fragment" },
      { type: "text", text: "searchable second fragment" },
    ],
  });
}
async function seedTask(store) {
  await store.tasks.createTask({
    taskId: "task",
    scopeId: "chat",
    kind: "chat.turn",
    payload: { durable: true },
  });
  const claim = await store.tasks.claimNext({
    ownerId: "worker",
    now: new Date(),
    scopesBusy: [],
  });
  await store.tasks.appendEvents(
    "task",
    [0, 1].map((seq) => ({
      type: "durability.event",
      seq,
      eventId: `event-${seq}`,
      timestamp: "2026-10-06T00:00:00.000Z",
      contractVersion: 1,
      attemptId: claim.attempt.attemptId,
    })),
    { leaseToken: claim.lease.leaseToken },
  );
}
async function seedProposal(store) {
  await store.proposals.create({
    id: "proposal",
    chatId: "chat",
    scopeKey: "document",
    actionId: "action",
    toolName: "edit",
    kind: "document.edit",
    risk: "low",
    envelope: { text: "keep" },
    operations: [{ insert: "keep" }],
    warnings: [],
    truncated: false,
    createdAt: "2026-10-06T00:00:00.000Z",
  });
  await store.proposals.transition("proposal", ["pending"], "approved");
  await store.proposals.transition("proposal", ["approved"], "applying", {
    operationId: "operation",
    claimedAt: "2026-10-06T00:00:01.000Z",
  });
  await store.proposals.recordOutcome("operation", {
    status: "partial",
    appliedOps: 1,
    failedOps: [{ opIndex: 1, error: "keep error" }],
  });
}

function rows(db, sql) {
  return db.query(sql).all();
}
function scalar(db, sql, key) {
  return Number(db.query(sql).get()[key]);
}
const migration = {
  from: SCHEMA_VERSION,
  to: SCHEMA_VERSION + 1,
  sql: "CREATE TABLE migration_receipts (id TEXT PRIMARY KEY); INSERT INTO migration_receipts VALUES ('once');",
};
function snapshot(db) {
  return Object.fromEntries(
    [
      "chats",
      "messages",
      "tasks",
      "task_attempts",
      "leases",
      "task_events",
      "proposals",
      "proposal_outcomes",
      "outbox",
      "fencing_counter",
    ].map((table) => [
      table,
      rows(db, `SELECT * FROM ${table} ORDER BY rowid`),
    ]),
  );
}
async function verifyPopulated(store) {
  assert.deepEqual(
    (await store.conversations.listMessages("chat")).map(
      (message) => message.id,
    ),
    ["message", "parts"],
  );
  assert.equal((await store.tasks.getTask("task")).status, "running");
  assert.deepEqual(
    (await store.tasks.listEvents("task")).map((event) => event.seq),
    [0, 1],
  );
  assert.equal(await store.tasks.nextSeq("task"), 2);
  assert.equal(
    (await store.proposals.get("proposal")).operationId,
    "operation",
  );
  assert.equal(
    (await store.proposals.getOutcome("operation")).failedOps[0].error,
    "keep error",
  );
  assert.equal(
    (await store.conversations.searchMessages("searchable")).length,
    1,
  );
}

function upgradeOptions(step = migration, targetVersion = step.to) {
  return { targetVersion, migrations: [...SQLITE_MIGRATIONS, step] };
}

export {
  bun,
  Store,
  raw,
  scratch,
  seed,
  rows,
  scalar,
  snapshot,
  verifyPopulated,
  SCHEMA_VERSION,
  migration,
  upgradeOptions,
};
