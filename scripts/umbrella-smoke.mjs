#!/usr/bin/env node
/**
 * Umbrella packaging smoke test: `npm pack` the `agentkit` umbrella package
 * built by `scripts/build-umbrella.mjs`, then install that ONE tarball into
 * two fresh throwaway projects — one with plain `npm install`, one with
 * `bun add` — and, in each, import every subpath under plain `node` and run
 * one functional check per subpath. This is the proof this repo's whole
 * distribution model rests on: "install `agentkit` as one package and every
 * `agentkit/<x>` subpath import works", under both package managers a
 * consumer might reasonably use.
 *
 * `agentkit/adapters-sqlite` is `bun:sqlite`-backed and must NOT be imported
 * under plain Node — it is checked separately, with a `bun` script, inside
 * the bun-installed project only.
 *
 * This script builds nothing itself:
 *
 *   bun run build && bun run build:umbrella && node scripts/umbrella-smoke.mjs
 *   # or: bun run build && bun run build:umbrella && bun run smoke:umbrella
 *
 * Not part of `bun run ci` — like `pack-smoke`, it shells out to real `npm
 * pack`/`npm install`/`bun add` (network access for agentkit's real,
 * non-workspace dependencies: typebox, ajv, the MCP SDK). It has its own CI
 * job, `umbrella-smoke`, which runs after `build`.
 */
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../", import.meta.url));
const UMBRELLA_DIR = join(ROOT, "packages", "agentkit");

function readPkg(dir) {
  return JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
}

function writePkg(dir, pkg) {
  writeFileSync(join(dir, "package.json"), `${JSON.stringify(pkg, null, 2)}\n`);
}

/** `npm pack` output filenames follow this deterministic rule: `name@version` → `name-version.tgz`. */
function expectedTarballName(pkg) {
  const unscoped = pkg.name.replace(/^@/, "").replace(/\//g, "-");
  return `${unscoped}-${pkg.version}.tgz`;
}

/** Runs a command, returning {ok, output}. Never throws — the caller decides how a failure is reported. */
function run(cmd, args, opts) {
  const result = spawnSync(cmd, args, { encoding: "utf8", ...opts });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (result.error) {
    return { ok: false, output: `${output}\n${result.error.message}` };
  }
  return { ok: result.status === 0, output, status: result.status };
}

/** One functional check per Node-importable subpath, run inside a fresh consumer project. */
function nodeCheckScript() {
  return `
const failures = [];
function check(condition, message) {
  if (condition) console.log(\`  ok   \${message}\`);
  else { console.error(\`  FAIL \${message}\`); failures.push(message); }
}

const contracts = await import("agentkit/contracts");
console.log("agentkit/contracts");
check(typeof contracts.CONTRACT_VERSION === "string", "exports CONTRACT_VERSION");

const core = await import("agentkit/core");
console.log("agentkit/core");
check(typeof core.runChat === "function", "exports runChat");
const stubProvider = {
  id: "stub",
  kind: "openai-compatible",
  async capabilities() { return { streaming: true, toolCalling: false, modelList: false }; },
  async listModels() { return []; },
  async *streamChat(input) {
    const stamp = core.createEventStamper({});
    const at = new Date().toISOString();
    yield stamp({ type: "run.started", runId: input.runId, timestamp: at, data: { model: input.model, toolCount: 0 } });
    yield stamp({ type: "run.message.completed", runId: input.runId, timestamp: at, data: { content: "hi", toolCallCount: 0, finishReason: "stop" } });
  },
};
const gen = core.runChat({
  client: stubProvider,
  registry: new core.AiToolRegistry(),
  model: "stub-model",
  messages: [{ role: "user", content: "hi" }],
  limits: core.resolveToolLimits({ preference: "small" }),
});
let result;
for (;;) {
  const next = await gen.next();
  if (next.done) { result = next.value; break; }
}
check(result?.terminal === "completed", \`runChat terminal is "\${result?.terminal}"\`);

const host = await import("agentkit/host");
console.log("agentkit/host");
check(typeof host.TurnRunner === "function", "exports TurnRunner");
check(
  host.isTaskTransitionAllowed("queued", "running") === true,
  "task transition table answers",
);

const testing = await import("agentkit/testing");
console.log("agentkit/testing");
const trace = testing.loadGoldenTrace("chat-only");
check(Array.isArray(trace) && trace.length > 0, \`loadGoldenTrace("chat-only") returned \${trace?.length ?? 0} event(s)\`);

const mcp = await import("agentkit/mcp-client");
console.log("agentkit/mcp-client");
const nullSecrets = { async get() { return null; }, async set() {}, async delete() {}, async listRefs() { return []; } };
const manager = new mcp.McpClientManager({ secrets: nullSecrets }, [
  { alias: "gh", transport: { kind: "stdio", command: "gh-mcp" } },
]);
check(typeof mcp.McpClientManager === "function", "exports McpClientManager");
check(manager.aliases().join(",") === "gh", "manager registers its configured alias");

const transport = await import("agentkit/transport-http");
console.log("agentkit/transport-http");
check(typeof transport.createRestHandler === "function", "exports createRestHandler");
const handler = transport.createRestHandler({
  store: {},
  turns: { async submitMessage() { throw new Error("unreachable"); } },
  tasks: { async cancelTask() { throw new Error("unreachable"); } },
  packages: { agentkit: "umbrella-smoke" },
});
const versionResponse = await handler(new Request("http://x/v1/version"));
check(versionResponse.status === 200, \`GET /v1/version answered \${versionResponse.status}\`);

const mcpServer = await import("agentkit/mcp-server");
console.log("agentkit/mcp-server");
check(typeof mcpServer.createMcpServerHandler === "function", "exports createMcpServerHandler");
check(typeof mcpServer.createStagedToolSource === "function", "exports createStagedToolSource");
const mcpServerHandler = mcpServer.createMcpServerHandler({
  tools: {
    catalog: { async listTools() { return []; } },
    async execute() { throw new Error("unreachable"); },
  },
  auth: { bearerToken: "umbrella-smoke-token" },
});
const mcpUnauthorized = await mcpServerHandler.fetch(new Request("http://localhost/mcp", {
  method: "POST", headers: { host: "localhost" }, body: "{}",
}));
check(mcpUnauthorized.status === 401, \`an unauthenticated POST answered \${mcpUnauthorized.status}\`);
const mcpRebound = await mcpServerHandler.fetch(new Request("http://localhost/mcp", {
  method: "POST", headers: { host: "evil.com", authorization: "Bearer umbrella-smoke-token" }, body: "{}",
}));
check(mcpRebound.status === 403, \`a rebound Host answered \${mcpRebound.status}\`);
await mcpServerHandler.dispose();

const adaptersMemory = await import("agentkit/adapters-memory");
console.log("agentkit/adapters-memory");
check(typeof adaptersMemory.MemoryAssistantStore === "function", "exports MemoryAssistantStore");
const store = new adaptersMemory.MemoryAssistantStore();
await store.tasks.createTask({ taskId: "umbrella-smoke", kind: "umbrella.smoke", scopeId: "umbrella-scope", payload: {} });
const claim = await store.tasks.claimNext({ ownerId: "umbrella-smoke", now: new Date(), scopesBusy: [] });
check(claim?.task.status === "running" && typeof claim?.lease.leaseToken === "string", "claimNext hands back a running task with a lease");

const runnerLocal = await import("agentkit/runner-local");
console.log("agentkit/runner-local");
check(typeof runnerLocal.SingleProcessTaskRunner === "function", "exports SingleProcessTaskRunner");
await store.tasks.releaseLease(claim.lease.leaseToken);
await store.tasks.endAttempt({ attemptId: claim.attempt.attemptId, status: "completed" });
await store.tasks.transitionTask("umbrella-smoke", ["running"], "completed", { finishedAt: new Date().toISOString() });
await store.tasks.createTask({ taskId: "umbrella-smoke-run", kind: "umbrella.smoke", scopeId: "umbrella-scope-2", payload: {} });
const runner = new runnerLocal.SingleProcessTaskRunner({ store, pollMs: 5 });
const handle = await runner.startWorker({
  async execute({ taskId, attemptId }) {
    await store.tasks.transitionTask(taskId, ["running"], "completed", { finishedAt: new Date().toISOString() });
    await store.tasks.endAttempt({ attemptId, status: "completed" });
  },
}, { concurrency: 1 });
await runner.enqueue({ taskId: "umbrella-smoke-run", scopeId: "umbrella-scope-2" });
let ran = null;
for (const deadline = Date.now() + 5000; Date.now() < deadline; ) {
  ran = await store.tasks.getTask("umbrella-smoke-run");
  if (ran?.status === "completed") break;
  await new Promise((resolve) => setTimeout(resolve, 5));
}
await handle.stop();
check(ran?.status === "completed", \`the runner drove a queued task to "\${ran?.status}"\`);

if (failures.length > 0) {
  console.error(\`\\n\${failures.length} check(s) failed\`);
  process.exit(1);
}
`;
}

/** Bun-only check for agentkit/adapters-sqlite: must never be imported under Node. */
const BUN_SQLITE_CHECK_SCRIPT = `
const failures = [];
function check(condition, message) {
  if (condition) console.log(\`  ok   \${message}\`);
  else { console.error(\`  FAIL \${message}\`); failures.push(message); }
}

const sqlite = await import("agentkit/adapters-sqlite");
console.log("agentkit/adapters-sqlite (bun only)");
check(typeof sqlite.SqliteAssistantStore === "function", "exports SqliteAssistantStore");

const store = new sqlite.SqliteAssistantStore(":memory:");
await store.tasks.createTask({ taskId: "umbrella-smoke-sqlite", kind: "umbrella.smoke", scopeId: "umbrella-scope", payload: {} });
const claim = await store.tasks.claimNext({ ownerId: "umbrella-smoke", now: new Date(), scopesBusy: [] });
check(claim?.task.status === "running" && typeof claim?.lease.leaseToken === "string", "claimNext hands back a running task with a lease");
store.close();

if (failures.length > 0) {
  console.error(\`\\n\${failures.length} check(s) failed\`);
  process.exit(1);
}
`;

function main() {
  console.log("umbrella-smoke: packing the agentkit umbrella with npm\n");

  const distEntry = join(UMBRELLA_DIR, "dist", "contracts", "index.js");
  if (!existsSync(distEntry)) {
    console.error(
      "packages/agentkit/dist is missing or incomplete — run " +
        '"bun run build && bun run build:umbrella" first.',
    );
    process.exit(1);
  }

  const npmVersion = run("npm", ["--version"]);
  if (!npmVersion.ok) {
    console.error(
      "npm is not available in this environment — cannot run the umbrella smoke test.\n" +
        npmVersion.output,
    );
    process.exit(1);
  }
  const bunVersion = run("bun", ["--version"]);
  if (!bunVersion.ok) {
    console.error(
      "bun is not available in this environment — cannot run the umbrella smoke test.\n" +
        bunVersion.output,
    );
    process.exit(1);
  }
  console.log(
    `npm ${npmVersion.output.trim()}, bun ${bunVersion.output.trim()}, node ${process.version}\n`,
  );

  const workDir = mkdtempSync(join(tmpdir(), "agentkit-umbrella-smoke-"));
  const tarballDir = join(workDir, "tarball");
  const npmProjectDir = join(workDir, "npm-consumer");
  const bunProjectDir = join(workDir, "bun-consumer");
  mkdirSync(tarballDir, { recursive: true });
  mkdirSync(npmProjectDir, { recursive: true });
  mkdirSync(bunProjectDir, { recursive: true });

  try {
    const umbrellaPkg = readPkg(UMBRELLA_DIR);
    console.log(`packing agentkit@${umbrellaPkg.version}...`);
    const packed = run("npm", ["pack", "--pack-destination", tarballDir], {
      cwd: UMBRELLA_DIR,
    });
    if (!packed.ok) {
      console.error(`npm pack failed for agentkit:\n${packed.output}`);
      process.exit(1);
    }
    const tarballPath = join(tarballDir, expectedTarballName(umbrellaPkg));
    if (!existsSync(tarballPath)) {
      console.error(
        `npm pack reported success but ${tarballPath} does not exist — ` +
          "expectedTarballName()'s naming guess is stale.\n" +
          packed.output,
      );
      process.exit(1);
    }
    console.log(`  ok   packed ${tarballPath}\n`);

    // --- npm install ---------------------------------------------------
    console.log("installing the tarball with npm install...");
    writePkg(npmProjectDir, {
      name: "agentkit-umbrella-smoke-npm",
      private: true,
      version: "0.0.0",
      type: "module",
    });
    const npmInstalled = run(
      "npm",
      ["install", tarballPath, "--no-audit", "--no-fund"],
      { cwd: npmProjectDir },
    );
    if (!npmInstalled.ok) {
      console.error(`npm install failed:\n${npmInstalled.output}`);
      process.exit(1);
    }
    console.log("  ok   npm install succeeded");

    writeFileSync(join(npmProjectDir, "smoke.mjs"), nodeCheckScript());
    const npmChecked = run("node", ["smoke.mjs"], { cwd: npmProjectDir });
    console.log(`\n${npmChecked.output.trim()}`);
    if (!npmChecked.ok) {
      console.error(
        "\numbrella-smoke FAILED — npm-installed checks did not pass.",
      );
      process.exit(1);
    }

    // --- bun add ---------------------------------------------------------
    console.log("\ninstalling the tarball with bun add...");
    writePkg(bunProjectDir, {
      name: "agentkit-umbrella-smoke-bun",
      private: true,
      version: "0.0.0",
      type: "module",
    });
    const bunAdded = run("bun", ["add", tarballPath], { cwd: bunProjectDir });
    if (!bunAdded.ok) {
      console.error(`bun add failed:\n${bunAdded.output}`);
      process.exit(1);
    }
    console.log("  ok   bun add succeeded");

    writeFileSync(join(bunProjectDir, "smoke.mjs"), nodeCheckScript());
    const bunProjectNodeChecked = run("node", ["smoke.mjs"], {
      cwd: bunProjectDir,
    });
    console.log(`\n${bunProjectNodeChecked.output.trim()}`);
    if (!bunProjectNodeChecked.ok) {
      console.error(
        "\numbrella-smoke FAILED — bun-installed checks under node did not pass.",
      );
      process.exit(1);
    }

    // agentkit/adapters-sqlite: bun-only, checked with a bun script against
    // the bun-installed project only — never imported under plain node.
    writeFileSync(
      join(bunProjectDir, "smoke-sqlite.mjs"),
      BUN_SQLITE_CHECK_SCRIPT,
    );
    const sqliteChecked = run("bun", ["run", "smoke-sqlite.mjs"], {
      cwd: bunProjectDir,
    });
    console.log(`\n${sqliteChecked.output.trim()}`);
    if (!sqliteChecked.ok) {
      console.error(
        "\numbrella-smoke FAILED — bun-only adapters-sqlite check did not pass.",
      );
      process.exit(1);
    }

    console.log("\numbrella-smoke passed");
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
}

main();
