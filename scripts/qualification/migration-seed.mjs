import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite-node";
import { exerciseNodeHost } from "./node-host.mjs";
const path = process.argv[2];
const outcome = await exerciseNodeHost(path);
const store = new NodeSqliteAssistantStore(path);
try {
  const schema = store.database.query("PRAGMA user_version").get().user_version;
  assert.equal(schema, 8);
  const messages = await store.conversations.listMessages(outcome.chatId);
  assert.ok(messages.length >= 2);
  const seed = {
    ...outcome,
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
