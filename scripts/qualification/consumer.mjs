import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { realpathSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { build } from "esbuild";
import { exerciseNodeHost } from "./node-host.mjs";

const paths = [
  "contracts",
  "core",
  "host",
  "adapters-sqlite-node",
  "runner-local",
  "transport-http",
  "client",
  "react",
  "mcp-client",
  "mcp-server",
  "adapters-memory",
  "testing",
];
for (const path of ["", ...paths.map((path) => `/${path}`)]) {
  const entry = await import(`agentkit${path}`);
  assert.ok(Object.keys(entry).length > 0, `Empty agentkit${path}`);
}

function entryFor(paths) {
  return paths
    .map(
      (path, i) =>
        `import * as entry${i} from 'agentkit/${path}'; export { entry${i} };`,
    )
    .join("\n");
}

writeFileSync("browser-entry.mjs", entryFor(["contracts", "client", "react"]));
const browser = await build({
  entryPoints: ["browser-entry.mjs"],
  bundle: true,
  platform: "browser",
  format: "esm",
  outfile: "browser.js",
  metafile: true,
});
for (const input of Object.keys(browser.metafile.inputs)) {
  assert.ok(
    !/bun:|better-sqlite3|adapters-sqlite|\.node$/.test(input),
    `Native browser input: ${input}`,
  );
}
writeFileSync(
  "browser-metafile.json",
  JSON.stringify(browser.metafile, null, 2),
);

writeFileSync(
  "electron-style-entry.mjs",
  entryFor(paths.filter((path) => path !== "testing")),
);
const cjs = await build({
  entryPoints: ["electron-style-entry.mjs"],
  bundle: true,
  platform: "node",
  target: "node22",
  format: "cjs",
  outfile: "electron-style.cjs",
  external: ["better-sqlite3"],
  metafile: true,
});
assert.ok(
  !Object.keys(cjs.metafile.inputs).some((input) =>
    /bun:|bun-driver|\.node$/.test(input),
  ),
);
assert.ok(
  Object.values(cjs.metafile.outputs)
    .flatMap((item) => item.imports)
    .some((item) => item.path === "better-sqlite3" && item.external),
);
writeFileSync(
  "electron-style-metafile.json",
  JSON.stringify(cjs.metafile, null, 2),
);
const require = createRequire(import.meta.url);
assert.ok(
  realpathSync(fileURLToPath(import.meta.resolve("agentkit"))).startsWith(
    join(realpathSync(process.cwd()), "node_modules", "agentkit"),
  ),
  "Installed package must resolve inside the clean consumer",
);
const bundled = require("./electron-style.cjs");
assert.equal(typeof bundled.entry3.NodeSqliteAssistantStore, "function");
let store = new bundled.entry3.NodeSqliteAssistantStore("bundled.sqlite");
await store.tasks.createTask({
  taskId: "bundle-task",
  kind: "qualification",
  scopeId: "bundle",
  payload: {},
});
store.close();
store = new bundled.entry3.NodeSqliteAssistantStore("bundled.sqlite");
assert.equal((await store.tasks.getTask("bundle-task"))?.taskId, "bundle-task");
store.close();
const nativeBinaries = Object.keys(require.cache).filter((path) =>
  path.endsWith(".node"),
);
assert.ok(
  nativeBinaries.some((path) => path.includes("better-sqlite3")),
  "Native addon did not load",
);
console.log(
  JSON.stringify({
    nativeDriver: require.resolve("better-sqlite3"),
    nativeBinaries,
    platform: process.platform,
    arch: process.arch,
    nodeAbi: process.versions.modules,
    cjsNativeReopened: true,
    electronRuntimeQualified: false,
  }),
);
console.log(JSON.stringify(await exerciseNodeHost("host.sqlite")));
