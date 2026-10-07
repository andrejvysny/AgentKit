const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const { readFileSync, realpathSync } = require("node:fs");
const { entry3: sqlite } = require("./electron-style.cjs");

async function seed(store) {
  await store.conversations.createChat({ id: "native-chat" });
  await store.conversations.appendMessage({
    id: "native-message",
    chatId: "native-chat",
    role: "user",
    content: "durable native fixture",
  });
  await store.tasks.createTask({
    taskId: "electron-task",
    kind: "qualification",
    scopeId: "electron",
    payload: { runtime: process.versions.electron ?? process.versions.node },
  });
}

async function verify(store) {
  assert.equal(
    (await store.tasks.getTask("electron-task"))?.taskId,
    "electron-task",
  );
  assert.equal(
    (await store.conversations.listMessages("native-chat"))[0].content,
    "durable native fixture",
  );
}

function metadata(phase) {
  const binaries = Object.keys(require.cache).filter((path) =>
    path.endsWith(".node"),
  );
  assert.ok(binaries.some((path) => path.includes("better-sqlite3")));
  const metadata = require("better-sqlite3/package.json");
  return {
    runtime: process.versions.electron ? "Electron" : "Node",
    electron: process.versions.electron,
    executable: process.execPath,
    node: process.versions.node,
    modules: process.versions.modules,
    napi: process.versions.napi,
    platform: process.platform,
    arch: process.arch,
    reopened: true,
    processReopened: phase === "reopen",
    nativePackage: {
      version: metadata.version,
      gypfile: metadata.gypfile,
      scripts: metadata.scripts,
    },
    binaries: binaries.map((path) => ({
      path: realpathSync(path),
      sha256: createHash("sha256")
        .update(readFileSync(realpathSync(path)))
        .digest("hex"),
    })),
  };
}

async function main() {
  if (process.env.AGENTKIT_EXPECT_RUNTIME !== "node")
    assert.ok(process.versions.electron, "Expected actual Electron runtime");
  const version = require("better-sqlite3/package.json").version;
  if (Number(version.split(".")[0]) >= 13)
    assert.ok(
      Number(process.versions.napi) >= 10,
      "Driver requires Node-API 10",
    );
  const path = process.argv[2];
  const phase = process.argv[3] ?? "seed";
  assert.ok(["seed", "reopen"].includes(phase), "Unknown native fixture phase");
  let store = new sqlite.NodeSqliteAssistantStore(path);
  if (phase === "seed") await seed(store);
  store.close();
  store = new sqlite.NodeSqliteAssistantStore(path);
  await verify(store);
  store.close();
  console.log(JSON.stringify(metadata(phase)));
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
