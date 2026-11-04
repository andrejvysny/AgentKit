// The schema-10 seed producer for migration qualification.
//
// The migration cell proves the frozen TypeScript adapter lineage (schema
// 10) survives into the native release: this script — run from a consumer
// of the PREVIOUS package — writes a schema-10 database through the
// adapter's own write path, and the candidate's `responses-smoke.mjs`
// reads it back through the adapter's read path (never touched by any
// native handle) before running its full native phase on a sidecar file.
//
// The seed is written directly (createChat + appendMessage): the old run
// through the retired TypeScript TurnRunner/TaskService stack was only
// ever observed here as the durable chat + messages the seed JSON records,
// so the durable records are produced without resurrecting the runner.
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite-node";

const path = process.argv[2];
const chatId = "migration-seed-chat";
const store = new NodeSqliteAssistantStore(path);
try {
  await store.conversations.createChat({ id: chatId, title: "Migration seed" });
  await store.conversations.appendMessage({
    id: "migration-seed-user",
    chatId,
    role: "user",
    content: "Migration seed prompt",
  });
  await store.conversations.appendMessage({
    id: "migration-seed-assistant",
    chatId,
    role: "assistant",
    content: "Migration seed reply",
  });
  const schema = store.database.query("PRAGMA user_version").get().user_version;
  assert.equal(schema, 10);
  const messages = await store.conversations.listMessages(chatId);
  assert.ok(messages.length >= 2);
  const seed = {
    chatId,
    terminal: "seeded",
    reopened: true,
    schema,
    messages: messages.map((message) => ({
      id: message.id,
      content: message.content,
    })),
  };
  writeFileSync(`${path}.seed.json`, `${JSON.stringify(seed, null, 2)}\n`);
  console.log(JSON.stringify(seed));
} finally {
  store.close();
}
