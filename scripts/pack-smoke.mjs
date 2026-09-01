#!/usr/bin/env node
/**
 * Packaging smoke test: `npm pack` every publishable `@agentkit/*` package,
 * install the tarballs into a fresh throwaway project with plain `npm`, then
 * import each package's PUBLISHED entry and run one functional check per
 * package (the same checks `scripts/node-smoke.mjs` runs against the
 * in-repo dists — this script is the same proof one hop further out: not
 * "does the dist load from the checkout", but "does `npm install
 * @agentkit/x` actually work").
 *
 * Run `bun run build` first; this reads `dist/`, which is not checked in.
 * Then:
 *
 *   node scripts/pack-smoke.mjs      # or: bun run smoke:pack
 *
 * NOT part of the normal `bun run ci` gate — it shells out to real `npm
 * pack`/`npm install` (network access for each package's real, non-workspace
 * dependencies — typebox, ajv, the MCP SDK), which is slower and less
 * hermetic than the rest of this repo's checks. It has its own CI job.
 *
 * WHY THIS CATCHES BUGS `bun test`/`node-smoke` CANNOT — two real,
 * confirmed-by-running-it npm behaviors this repo's package.jsons assumed
 * away:
 *
 *   1. `publishConfig.exports/.main/.types` overriding the top-level fields
 *      on `npm pack`/`npm publish` is PNPM-SPECIFIC — plain npm packs the
 *      top-level fields verbatim (confirmed against npm 10.9.8: packing
 *      `@agentkit/contracts` as-is and inspecting the tarball's
 *      `package.json` shows `exports["."]` still pointing at
 *      `./src/index.ts`, not `publishConfig`'s `./dist/index.js`; tracked
 *      upstream as still-open https://github.com/npm/cli/issues/7586). Left
 *      alone, every published package would ship an entry point pointing at
 *      TypeScript source a plain Node/npm consumer cannot load. Fixed at the
 *      source: every publishable package's `package.json` now runs
 *      `scripts/prepack-publish-config.mjs` as its `prepack`/`postpack` —
 *      see that file. This script's `npm pack` calls exercise that fix for
 *      real, the same way `npm publish` would.
 *   2. `workspace:*` dependency ranges are not something plain npm
 *      understands at all — only a tool that natively speaks the workspace
 *      protocol (bun, pnpm, yarn) rewrites them at publish time. This script
 *      does that rewrite itself, for the duration of each `npm pack` call
 *      only (backed up and restored around it — see `withRewrittenDeps`),
 *      pointing each `workspace:*` entry at the sibling tarball this script
 *      already packed. This is deliberately scoped to this test, not fixed
 *      at the source the way (1) was: which tool actually cuts a release is
 *      an open question this repo has not answered yet (see README's "Not
 *      published to npm" note), and `bun publish` already rewrites
 *      `workspace:*` correctly on its own — there is nothing here to fix
 *      until a release tool is chosen.
 *
 * A THIRD bug this style of test already caught once, fixed permanently
 * rather than worked around: `packages/testing/src/golden/golden.ts`
 * imported its golden-trace JSON files without `with { type: "json" }`,
 * which `bun test` never notices (bun does not require the attribute) but
 * which throws `ERR_IMPORT_ATTRIBUTE_MISSING` under plain Node — see
 * `scripts/node-smoke.mjs`'s testing-dist check, which now also catches it
 * at the cheaper (no `npm install`) layer.
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

/** Dependency order — a package may only depend on ones earlier in this list. */
const PACKAGES = [
  "contracts",
  "client",
  "react",
  "core",
  "host",
  // Ahead of the adapters: both `adapters-memory` and `adapters-sqlite` depend
  // on it, and `withRewrittenDeps` can only point a `workspace:*` at a tarball
  // an EARLIER entry already packed.
  "mcp-client",
  "adapters-memory",
  "adapters-sqlite",
  "runner-local",
  "testing",
  "transport-http",
  "mcp-server",
];

/**
 * Packages the consumer check script does NOT import. `npm pack` + `npm
 * install` still cover them — the packed `exports` must point into `dist/` and
 * the tarball must install — but `@agentkit/adapters-sqlite` is built on
 * `bun:sqlite` and declares `engines.bun` only, so importing it from a plain
 * `node check.mjs` would fail by design rather than find a bug.
 */
const NOT_NODE_IMPORTABLE = new Set(["@agentkit/adapters-sqlite"]);

/**
 * Peer dependencies the consumer project must install for the packages above to
 * be importable at all.
 *
 * `@agentkit/react` declares `react` as a PEER — it must not bundle or pin the
 * consumer's React — so nothing in the tarball graph brings one, and the
 * consumer has to. Installing it here is also the only way the check script
 * below can load that package's dist at all.
 */
const PEER_DEPENDENCIES = { react: "^19.2.0" };

function readPkg(dir) {
  return JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
}

function writePkg(dir, pkg) {
  writeFileSync(join(dir, "package.json"), `${JSON.stringify(pkg, null, 2)}\n`);
}

/** Runs `fn` with this package's `workspace:*` deps pointed at already-packed sibling tarballs, then restores the file. */
function withRewrittenDeps(dir, tarballByName, fn) {
  const original = readFileSync(join(dir, "package.json"), "utf8");
  const pkg = JSON.parse(original);
  let rewrote = false;
  for (const field of ["dependencies", "peerDependencies"]) {
    const deps = pkg[field];
    if (!deps) continue;
    for (const [name, range] of Object.entries(deps)) {
      if (range !== "workspace:*") continue;
      const tarball = tarballByName.get(name);
      if (tarball === undefined) {
        throw new Error(
          `${dir}/package.json depends on ${name}@workspace:* but no earlier ` +
            `package in PACKAGES packed a tarball for it — is PACKAGES ordering wrong?`,
        );
      }
      deps[name] = `file:${tarball}`;
      rewrote = true;
    }
  }
  if (rewrote) writePkg(dir, pkg);
  try {
    return fn();
  } finally {
    if (rewrote) writeFileSync(join(dir, "package.json"), original);
  }
}

/** `npm pack` output filenames follow this deterministic rule: `@scope/name@version` → `scope-name-version.tgz`. */
function expectedTarballName(pkg) {
  const unscoped = pkg.name.replace(/^@/, "").replace(/\//g, "-");
  return `${unscoped}-${pkg.version}.tgz`;
}

/** Runs a command, returning {ok, output}. Never throws — the caller decides how a failure is reported. */
function run(cmd, args, opts) {
  const result = spawnSync(cmd, args, {
    encoding: "utf8",
    ...opts,
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  if (result.error) {
    return { ok: false, output: `${output}\n${result.error.message}` };
  }
  return { ok: result.status === 0, output, status: result.status };
}

function main() {
  console.log("pack-smoke: packing every publishable package with npm\n");

  for (const name of PACKAGES) {
    const distPath = join(ROOT, "packages", name, "dist", "index.js");
    if (!existsSync(distPath)) {
      console.error(
        `packages/${name}/dist/index.js is missing — run "bun run build" first.`,
      );
      process.exit(1);
    }
  }

  const npmVersion = run("npm", ["--version"]);
  if (!npmVersion.ok) {
    console.error(
      "npm is not available in this environment — cannot run the packaging smoke test.\n" +
        npmVersion.output,
    );
    process.exit(1);
  }
  console.log(`npm ${npmVersion.output.trim()}, node ${process.version}\n`);

  const workDir = mkdtempSync(join(tmpdir(), "agentkit-pack-smoke-"));
  const tarballDir = join(workDir, "tarballs");
  const consumerDir = join(workDir, "consumer");
  mkdirSync(tarballDir, { recursive: true });
  const tarballByName = new Map(); // "@agentkit/x" -> absolute tarball path

  try {
    for (const name of PACKAGES) {
      const pkgDir = join(ROOT, "packages", name);
      const pkg = readPkg(pkgDir);
      console.log(`packing @agentkit/${name}@${pkg.version}...`);

      const packed = withRewrittenDeps(pkgDir, tarballByName, () =>
        run("npm", ["pack", "--pack-destination", tarballDir], {
          cwd: pkgDir,
        }),
      );
      if (!packed.ok) {
        console.error(
          `npm pack failed for @agentkit/${name}:\n${packed.output}`,
        );
        process.exit(1);
      }

      const tarballPath = join(tarballDir, expectedTarballName(pkg));
      if (!existsSync(tarballPath)) {
        console.error(
          `npm pack for @agentkit/${name} reported success but ${tarballPath} ` +
            `does not exist — expectedTarballName()'s naming guess is stale.\n` +
            packed.output,
        );
        process.exit(1);
      }
      tarballByName.set(pkg.name, tarballPath);

      // Verify the fix this whole script exists to check: the PACKED
      // package.json (inside the tarball, not the working tree) must resolve
      // "." to dist/, not src/.
      const extracted = run("tar", [
        "-xzOf",
        tarballPath,
        "package/package.json",
      ]);
      if (!extracted.ok) {
        console.error(
          `could not read packed package.json for @agentkit/${name}`,
        );
        process.exit(1);
      }
      const packedPkg = JSON.parse(extracted.output);
      const entry = packedPkg.exports?.["."]?.import ?? packedPkg.main;
      if (
        typeof entry !== "string" ||
        !entry.replace(/^\.\//, "").startsWith("dist/")
      ) {
        console.error(
          `@agentkit/${name}'s packed package.json exports "." as ${JSON.stringify(entry)} — ` +
            "does not point into dist/. A real npm consumer cannot load this.",
        );
        process.exit(1);
      }
      console.log(`  ok   packed exports point at ${entry}`);
      if (NOT_NODE_IMPORTABLE.has(pkg.name)) {
        console.log("  note installed but not imported below — Bun-only");
      }
    }

    console.log(
      "\ninstalling every tarball into a fresh project with npm install...",
    );
    const consumerPkg = {
      name: "agentkit-pack-smoke-consumer",
      private: true,
      version: "0.0.0",
      type: "module",
      dependencies: {
        ...Object.fromEntries(
          [...tarballByName].map(([name, path]) => [name, `file:${path}`]),
        ),
        ...PEER_DEPENDENCIES,
      },
    };
    mkdirSync(consumerDir, { recursive: true });
    writePkg(consumerDir, consumerPkg);

    const installed = run("npm", ["install", "--no-audit", "--no-fund"], {
      cwd: consumerDir,
    });
    if (!installed.ok) {
      console.error(
        `npm install failed in the consumer project:\n${installed.output}`,
      );
      process.exit(1);
    }
    console.log("  ok   npm install succeeded");

    writeFileSync(join(consumerDir, "check.mjs"), CHECK_SCRIPT);
    const checked = run("node", ["check.mjs"], { cwd: consumerDir });
    console.log(`\n${checked.output.trim()}`);
    if (!checked.ok) {
      console.error("\npack-smoke FAILED — functional checks did not pass.");
      process.exit(1);
    }

    console.log("\npack-smoke passed");
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
}

/** One functional check per package, run inside the fresh consumer project (own node_modules, real npm resolution). */
const CHECK_SCRIPT = `
const failures = [];
function check(condition, message) {
  if (condition) console.log(\`  ok   \${message}\`);
  else { console.error(\`  FAIL \${message}\`); failures.push(message); }
}

const contracts = await import("@agentkit/contracts");
console.log("@agentkit/contracts");
check(typeof contracts.CONTRACT_VERSION === "string", "exports CONTRACT_VERSION");

const clientPkg = await import("@agentkit/client");
console.log("@agentkit/client");
check(typeof clientPkg.createAgentKitClient === "function", "exports createAgentKitClient");
const restClient = clientPkg.createAgentKitClient({ baseUrl: "http://127.0.0.1:1" });
check(
  Object.keys(contracts.REST_ROUTES).every((op) => typeof restClient[op] === "function"),
  "has a method for every contract route",
);
check(clientPkg.runPhase({ status: "queued" }) === "queued", "runPhase mirrors a status");

const reactPkg = await import("@agentkit/react");
console.log("@agentkit/react");
check(typeof reactPkg.AgentKitProvider === "function", "exports AgentKitProvider");
check(
  ["useChat", "useRun", "useBranches", "useProposals", "useProviders"].every(
    (hook) => typeof reactPkg[hook] === "function",
  ),
  "exports every hook",
);
check(typeof reactPkg.createChangeEmitter === "function", "exports createChangeEmitter");

const core = await import("@agentkit/core");
console.log("@agentkit/core");
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

const host = await import("@agentkit/host");
console.log("@agentkit/host");
check(
  host.isTaskTransitionAllowed("queued", "running") === true,
  "task transition table answers",
);

const adaptersMemory = await import("@agentkit/adapters-memory");
console.log("@agentkit/adapters-memory");
const store = new adaptersMemory.MemoryAssistantStore();
await store.tasks.createTask({ taskId: "pack-smoke", kind: "pack.smoke", scopeId: "pack-scope", payload: {} });
const claim = await store.tasks.claimNext({ ownerId: "pack-smoke", now: new Date(), scopesBusy: [] });
check(claim?.task.status === "running" && typeof claim?.lease.leaseToken === "string", "claimNext hands back a running task with a lease");

const runnerLocal = await import("@agentkit/runner-local");
console.log("@agentkit/runner-local");
const runner = new runnerLocal.SingleProcessTaskRunner({ store, pollMs: 5 });
await store.tasks.releaseLease(claim.lease.leaseToken);
await store.tasks.endAttempt({ attemptId: claim.attempt.attemptId, status: "completed" });
await store.tasks.transitionTask("pack-smoke", ["running"], "completed", { finishedAt: new Date().toISOString() });
await store.tasks.createTask({ taskId: "pack-smoke-run", kind: "pack.smoke", scopeId: "pack-scope-2", payload: {} });
const handle = await runner.startWorker({
  async execute({ taskId, attemptId }) {
    await store.tasks.transitionTask(taskId, ["running"], "completed", { finishedAt: new Date().toISOString() });
    await store.tasks.endAttempt({ attemptId, status: "completed" });
  },
}, { concurrency: 1 });
await runner.enqueue({ taskId: "pack-smoke-run", scopeId: "pack-scope-2" });
let ran = null;
for (const deadline = Date.now() + 5000; Date.now() < deadline; ) {
  ran = await store.tasks.getTask("pack-smoke-run");
  if (ran?.status === "completed") break;
  await new Promise((resolve) => setTimeout(resolve, 5));
}
await handle.stop();
check(ran?.status === "completed", \`the runner drove a queued task to "\${ran?.status}"\`);

const testing = await import("@agentkit/testing");
console.log("@agentkit/testing");
const trace = testing.loadGoldenTrace("chat-only");
check(Array.isArray(trace) && trace.length > 0, \`loadGoldenTrace("chat-only") returned \${trace?.length ?? 0} event(s)\`);

const mcp = await import("@agentkit/mcp-client");
console.log("@agentkit/mcp-client");
const nullSecrets = { async get() { return null; }, async set() {}, async delete() {}, async listRefs() { return []; } };
const manager = new mcp.McpClientManager({ secrets: nullSecrets }, [
  { alias: "gh", transport: { kind: "stdio", command: "gh-mcp" } },
]);
check(manager.aliases().join(",") === "gh", "manager registers its configured alias");

const transport = await import("@agentkit/transport-http");
console.log("@agentkit/transport-http");
const handler = transport.createRestHandler({
  store: {},
  turns: { async submitMessage() { throw new Error("unreachable"); } },
  tasks: { async cancelTask() { throw new Error("unreachable"); } },
  packages: { "@agentkit/transport-http": "pack-smoke" },
});
const versionResponse = await handler(new Request("http://x/v1/version"));
check(versionResponse.status === 200, \`GET /v1/version answered \${versionResponse.status}\`);

const mcpServer = await import("@agentkit/mcp-server");
console.log("@agentkit/mcp-server");
check(typeof mcpServer.createMcpServerHandler === "function", "exports createMcpServerHandler");
check(typeof mcpServer.createStagedToolSource === "function", "exports createStagedToolSource");
const mcpServerHandler = mcpServer.createMcpServerHandler({
  tools: {
    catalog: { async listTools() { return []; } },
    async execute() { throw new Error("unreachable"); },
  },
  auth: { bearerToken: "pack-smoke-token" },
});
const mcpUnauthorized = await mcpServerHandler.fetch(new Request("http://localhost/mcp", {
  method: "POST", headers: { host: "localhost" }, body: "{}",
}));
check(mcpUnauthorized.status === 401, \`an unauthenticated POST answered \${mcpUnauthorized.status}\`);
const mcpRebound = await mcpServerHandler.fetch(new Request("http://localhost/mcp", {
  method: "POST", headers: { host: "evil.com", authorization: "Bearer pack-smoke-token" }, body: "{}",
}));
check(mcpRebound.status === 403, \`a rebound Host answered \${mcpRebound.status}\`);
await mcpServerHandler.dispose();

if (failures.length > 0) {
  console.error(\`\\n\${failures.length} check(s) failed\`);
  process.exit(1);
}
`;

main();
