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
 * This script builds nothing itself:
 *
 *   bun run build && bun run build:umbrella && node scripts/umbrella-smoke.mjs
 *   # or: bun run build && bun run build:umbrella && bun run smoke:umbrella
 *
 * Not part of `bun run ci` — like `pack-smoke`, it shells out to real `npm
 * pack`/`npm install`/`bun add` (network access for agentkit's real,
 * non-workspace dependencies: typebox, ajv). It has its own CI
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

/**
 * The umbrella's one peer dependency, installed alongside the tarball.
 *
 * `agentkit/react` imports `react`, and the umbrella declares it as an OPTIONAL
 * peer — optional because the other subpaths have nothing to do with
 * React and an installer that only wants `agentkit/core` should not be told it
 * is missing something. Optional also means npm will NOT install it on its own,
 * so a consumer that wants the hooks installs React itself; this is that
 * consumer.
 */
const REACT_PEER = "react@^19.2.0";

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

const clientPkg = await import("agentkit/client");
console.log("agentkit/client");
check(typeof clientPkg.createAgentKitClient === "function", "exports createAgentKitClient");
check(typeof clientPkg.runPhase === "function", "exports runPhase");
const restClient = clientPkg.createAgentKitClient({ baseUrl: "http://127.0.0.1:1/api/" });
check(restClient.baseUrl === "http://127.0.0.1:1/api", "normalises the base URL");
check(typeof restClient.streamRun === "function", "has a streamRun method");
check(
  Object.keys(contracts.REST_ROUTES).every((op) => typeof restClient[op] === "function"),
  \`has a method for every one of the \${Object.keys(contracts.REST_ROUTES).length} contract routes\`,
);
check(
  clientPkg.runPhase({ status: "running", events: [{ type: "run.started" }] }) === "streaming",
  "runPhase derives streaming from the log",
);

// Imported, never rendered: there is no DOM and no react-dom here, and the
// failure this guards against is import-time anyway — a JSX-runtime import the
// build emitted, or a specifier the umbrella rewrite mangled on its way in.
// \`react\` itself is the peer installed alongside the tarball.
const reactPkg = await import("agentkit/react");
console.log("agentkit/react");
check(typeof reactPkg.AgentKitProvider === "function", "exports AgentKitProvider");
check(typeof reactPkg.useChat === "function", "exports useChat");
check(
  ["useRun", "useBranches", "useProposals", "useProviders"].every(
    (hook) => typeof reactPkg[hook] === "function",
  ),
  "exports the rest of the hooks",
);
const changeEmitter = reactPkg.createChangeEmitter();
let emitterCalls = 0;
const offEmitter = changeEmitter.subscribe(reactPkg.chatTopic("c1"), () => { emitterCalls += 1; });
changeEmitter.emit(reactPkg.chatTopic("c1"));
offEmitter();
changeEmitter.emit(reactPkg.chatTopic("c1"));
check(emitterCalls === 1, "the change emitter delivers once and unsubscribes");

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
// The Rust runtime is the only backend, and the private platform addon is not
// installable in this consumer: the default backend must fail HONESTLY about
// the missing binding. The packed functional run (full lifecycle plus
// fresh-process reopen) is qualified by rust/scripts/check-native-umbrella.mjs.
let runOutcome = "settled without a terminal result";
try {
  for (;;) {
    const next = await gen.next();
    if (next.done) {
      runOutcome = \`unexpectedly completed: \${next.value?.terminal}\`;
      break;
    }
  }
} catch (error) {
  runOutcome = String(error?.message ?? error);
}
check(
  runOutcome.includes("native binding failed to load"),
  \`runChat without the binding must fail honestly, got: \${runOutcome}\`,
);

const testing = await import("agentkit/testing");
console.log("agentkit/testing");
const trace = testing.loadGoldenTrace("chat-only");
check(Array.isArray(trace) && trace.length > 0, \`loadGoldenTrace("chat-only") returned \${trace?.length ?? 0} event(s)\`);

if (failures.length > 0) {
  console.error(\`\\n\${failures.length} check(s) failed\`);
  process.exit(1);
}
`;
}

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
      ["install", tarballPath, REACT_PEER, "--no-audit", "--no-fund"],
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
    const bunAdded = run("bun", ["add", tarballPath, REACT_PEER], {
      cwd: bunProjectDir,
    });
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

    console.log("\numbrella-smoke passed");
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
}

main();
