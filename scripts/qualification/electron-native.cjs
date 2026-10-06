const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const { readFileSync } = require("node:fs");
const { entry3: sqlite } = require("./electron-style.cjs");

async function main() {
  assert.ok(process.versions.electron, "Expected actual Electron runtime");
  assert.ok(Number(process.versions.napi) >= 10, "Driver requires Node-API 10");
  const path = process.argv[2];
  let store = new sqlite.NodeSqliteAssistantStore(path);
  await store.tasks.createTask({
    taskId: "electron-task",
    kind: "qualification",
    scopeId: "electron",
    payload: { runtime: process.versions.electron },
  });
  store.close();
  store = new sqlite.NodeSqliteAssistantStore(path);
  assert.equal(
    (await store.tasks.getTask("electron-task"))?.taskId,
    "electron-task",
  );
  store.close();
  const binaries = Object.keys(require.cache).filter((path) =>
    path.endsWith(".node"),
  );
  assert.ok(binaries.some((path) => path.includes("better-sqlite3")));
  const metadata = require("better-sqlite3/package.json");
  console.log(
    JSON.stringify({
      electron: process.versions.electron,
      executable: process.execPath,
      node: process.versions.node,
      modules: process.versions.modules,
      napi: process.versions.napi,
      platform: process.platform,
      arch: process.arch,
      reopened: true,
      nativePackage: {
        version: metadata.version,
        gypfile: metadata.gypfile,
        scripts: metadata.scripts,
      },
      binaries: binaries.map((path) => ({
        path,
        sha256: createHash("sha256").update(readFileSync(path)).digest("hex"),
      })),
    }),
  );
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
