/**
 * Node smoke test for the shippable dists.
 *
 * Bun is this repository's primary runtime, and that is exactly why this exists:
 * every published `@agentkit/*` package promises to be plain, portable
 * JavaScript, and nothing about running the test suite under Bun can prove it.
 * A `bun:sqlite` import that slipped into core, a `Bun.file()` call, an
 * accidental dependency on Bun's resolver — all of it passes `bun test` and
 * breaks the first Node consumer. So: plain Node, no Bun APIs, the built `dist`
 * output rather than the source.
 *
 * Five checks, end-to-end rather than smoke-in-name-only where a package has
 * something end-to-end to run:
 *
 *   1. Ajv (Node's, not Bun's) compiles `AiRunEventSchema` out of the contracts
 *      dist and validates a committed golden trace against it — the wire
 *      contract, exercised by the validator a real consumer would use.
 *   2. `runChat` from the core dist drives a stub provider to a terminal
 *      `completed`, stamping its events with core's own `createEventStamper`.
 *   3. The host dist loads and its port vocabulary answers — the transition
 *      table and the dependency gate are pure functions, so they run for real.
 *   4. The mcp-client dist loads (which drags in the MCP SDK under Node's
 *      resolver, not Bun's) and a manager constructs. Nothing connects: this is
 *      about the module graph and the constructor's eager validation.
 *   5. The transport-http dist serves `GET /v1/version` through a real
 *      `Request`/`Response` round trip against stub deps.
 *
 * RESOLUTION NOTE: each package's `exports` deliberately points at TypeScript
 * SOURCE during development (so a bundler-resolution typecheck reads the real
 * source), while `publishConfig.exports` records what a published install would
 * resolve to. Node loading a dist must follow the published mapping, so this
 * script registers a resolve hook that applies `publishConfig` to workspace
 * specifiers — simulating the installed layout rather than the checkout's.
 * Third-party specifiers (the MCP SDK) fall through to Node's own resolver,
 * which is the point: it has to find them from the repo's `node_modules`.
 *
 * Run `bun run build` first; the dists are not checked in. `bun run smoke:node`
 * from the repo root does the running part.
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { register } from "node:module";

const ROOT = new URL("../", import.meta.url);
const failures = [];

function check(condition, message) {
  if (condition) {
    console.log(`  ok   ${message}`);
  } else {
    console.error(`  FAIL ${message}`);
    failures.push(message);
  }
}

/** The entry a *published* install of this workspace package would resolve to. */
function publishedEntry(pkgDir) {
  const pkgUrl = new URL(`packages/${pkgDir}/package.json`, ROOT);
  const pkg = JSON.parse(readFileSync(pkgUrl, "utf8"));
  const entry = pkg.publishConfig?.exports?.["."]?.import;
  if (typeof entry !== "string") {
    console.error(
      `packages/${pkgDir}/package.json has no publishConfig.exports["."].import — ` +
        "a published consumer would have nothing to resolve.",
    );
    process.exit(1);
  }
  return new URL(`packages/${pkgDir}/${entry.replace(/^\.\//, "")}`, ROOT).href;
}

const CONTRACTS_ENTRY = publishedEntry("contracts");
const CORE_ENTRY = publishedEntry("core");
const HOST_ENTRY = publishedEntry("host");
const MCP_CLIENT_ENTRY = publishedEntry("mcp-client");
const TRANSPORT_HTTP_ENTRY = publishedEntry("transport-http");

// Apply the published mapping to every workspace specifier the dists import at
// runtime. Registered before any dynamic import below.
const WORKSPACE_ENTRIES = {
  "@agentkit/contracts": CONTRACTS_ENTRY,
  "@agentkit/core": CORE_ENTRY,
  "@agentkit/host": HOST_ENTRY,
  "@agentkit/mcp-client": MCP_CLIENT_ENTRY,
  "@agentkit/transport-http": TRANSPORT_HTTP_ENTRY,
};

register(
  "data:text/javascript," +
    encodeURIComponent(`
      const entries = ${JSON.stringify(WORKSPACE_ENTRIES)};
      export function resolve(specifier, context, next) {
        const mapped = entries[specifier];
        if (mapped !== undefined) return { url: mapped, shortCircuit: true };
        return next(specifier, context);
      }
    `),
);

// Ajv is a dependency of @agentkit/core, not of this script: resolve it the way
// core itself does rather than assuming the installer hoisted it to the root.
const requireFromCore = createRequire(new URL("packages/core/package.json", ROOT));
const ajvModule = requireFromCore("ajv");
const Ajv = ajvModule.default ?? ajvModule;

console.log(`node ${process.version} — smoke over the shippable dists`);

// ---------------------------------------------------------------------------
// 1. Contracts dist: Ajv compiles the event schema, a golden trace validates
// ---------------------------------------------------------------------------

const contracts = await import(CONTRACTS_ENTRY);
console.log("contracts dist");
check(
  typeof contracts.CONTRACT_VERSION === "string" &&
    /^\d+\.\d+\.\d+$/.test(contracts.CONTRACT_VERSION),
  `exports CONTRACT_VERSION (${contracts.CONTRACT_VERSION})`,
);
check(
  typeof contracts.AiRunEventSchema === "object" &&
    contracts.AiRunEventSchema !== null,
  "exports AiRunEventSchema",
);

const ajv = new Ajv({ strict: false, allErrors: true });
// TypeBox schemas carry symbol-keyed internals; normalizing through JSON is
// what a consumer reading them off the wire (or a .json dump) would get.
const validateEvent = ajv.compile(
  JSON.parse(JSON.stringify(contracts.AiRunEventSchema)),
);

const tracePath = new URL(
  "packages/testing/src/golden/traces/chat-only.json",
  ROOT,
);
const trace = JSON.parse(readFileSync(tracePath, "utf8"));
check(Array.isArray(trace) && trace.length > 0, "golden trace chat-only loaded");

const golden = trace[0];
const goldenValid = validateEvent(golden);
if (!goldenValid) {
  console.error(JSON.stringify(validateEvent.errors, null, 2));
}
check(goldenValid, `golden event "${golden?.type}" validates against AiRunEventSchema`);
// The negative half: a validator that accepts anything proves nothing.
check(
  validateEvent({ type: "run.exploded", runId: "r", seq: 0 }) === false,
  "AiRunEventSchema rejects an unknown event type",
);

// ---------------------------------------------------------------------------
// 2. Core dist: runChat drives a stub provider to a terminal completed
// ---------------------------------------------------------------------------

const core = await import(CORE_ENTRY);
console.log("core dist");
check(typeof core.runChat === "function", "exports runChat");
check(typeof core.AiToolRegistry === "function", "exports AiToolRegistry");
check(typeof core.createEventStamper === "function", "exports createEventStamper");

/** One completed text turn — the smallest provider that can end a run. */
const stubProvider = {
  id: "stub",
  kind: "openai-compatible",
  async capabilities() {
    return { streaming: true, toolCalling: false, modelList: false };
  },
  async listModels() {
    return [];
  },
  async *streamChat(input) {
    const stamp = core.createEventStamper({});
    const at = new Date().toISOString();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: at,
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: at,
      data: { delta: "Hello from Node." },
    });
    yield stamp({
      type: "run.message.completed",
      runId: input.runId,
      timestamp: at,
      data: {
        content: "Hello from Node.",
        toolCallCount: 0,
        finishReason: "stop",
      },
    });
  },
};

const generator = core.runChat({
  client: stubProvider,
  registry: new core.AiToolRegistry(),
  model: "stub-model",
  messages: [{ role: "user", content: "hi" }],
  limits: core.resolveToolLimits({ preference: "small" }),
});

const events = [];
let result;
for (;;) {
  const next = await generator.next();
  if (next.done) {
    result = next.value;
    break;
  }
  events.push(next.value);
}

check(events.length > 0, `runChat emitted ${events.length} events`);
check(result?.terminal === "completed", `runChat terminal is "${result?.terminal}"`);
check(
  events.every((event) => event.contractVersion === contracts.CONTRACT_VERSION),
  "every emitted event carries the contract version",
);
check(
  events.every((event, index) => event.seq === index),
  "seq is gapless from 0 across the run",
);
check(
  events.every((event) => validateEvent(event)),
  "every emitted event validates against AiRunEventSchema",
);

// ---------------------------------------------------------------------------
// 3. Host dist: the port vocabulary loads and answers
// ---------------------------------------------------------------------------

const host = await import(HOST_ENTRY);
console.log("host dist");
check(typeof host.TurnRunner === "function", "exports TurnRunner");
check(typeof host.TaskService === "function", "exports TaskService");
check(typeof host.ProposalService === "function", "exports ProposalService");
check(typeof host.AgentKitHostError === "function", "exports AgentKitHostError");
check(typeof host.defaultClock?.nowIso === "function", "exports a working defaultClock");
// Two pure functions the whole task system is graded against — cheap to run,
// and a broken one would mean the dist is not the code the tests exercised.
check(
  host.isTaskTransitionAllowed("queued", "running") === true &&
    host.isTaskTransitionAllowed("completed", "running") === false,
  "task transition table answers through the dist",
);
check(
  host.evaluateTaskDependencies([
    { taskId: "a", status: "failed", deadLettered: false },
  ]).kind === "settle",
  "dependency gate dooms a dependent of a failed task",
);

// ---------------------------------------------------------------------------
// 4. MCP client dist: the module graph resolves under Node, a manager builds
// ---------------------------------------------------------------------------

const mcp = await import(MCP_CLIENT_ENTRY);
console.log("mcp-client dist");
check(typeof mcp.McpClientManager === "function", "exports McpClientManager");
check(typeof mcp.createMcpToolSetContributor === "function", "exports createMcpToolSetContributor");
check(
  mcp.buildMcpToolIdentity({ serverAlias: "gh", toolName: "list_issues" })
    .registryName === "mcp__gh__list_issues",
  "canonical identity projects onto a registry-legal name",
);

// Constructed, never connected: the constructor validates aliases eagerly, and
// that is the part worth proving loads. A connect would need a real server.
const nullSecrets = {
  async get() {
    return null;
  },
  async set() {},
  async delete() {},
  async listRefs() {
    return [];
  },
};
const manager = new mcp.McpClientManager({ secrets: nullSecrets }, [
  { alias: "gh", transport: { kind: "stdio", command: "gh-mcp" } },
]);
check(manager.aliases().join(",") === "gh", "manager registers its configured alias");
check(manager.connectedAliases().length === 0, "manager starts with nothing connected");

// ---------------------------------------------------------------------------
// 5. Transport-http dist: a real Request/Response round trip
// ---------------------------------------------------------------------------

const transport = await import(TRANSPORT_HTTP_ENTRY);
console.log("transport-http dist");
check(typeof transport.createRestHandler === "function", "exports createRestHandler");

/** The narrowest deps `GET /v1/version` touches: none of them, plus `packages`. */
const restDeps = {
  store: {},
  turns: {
    async submitMessage() {
      throw new Error("unreachable");
    },
  },
  tasks: {
    async cancelTask() {
      throw new Error("unreachable");
    },
  },
  packages: { "@agentkit/transport-http": "node-smoke" },
};
const handler = transport.createRestHandler(restDeps);
const versionResponse = await handler(new Request("http://x/v1/version"));
check(versionResponse.status === 200, `GET /v1/version answered ${versionResponse.status}`);
const versionBody = await versionResponse.json();
check(
  versionBody.contractVersion === contracts.CONTRACT_VERSION,
  `version route reports contractVersion ${versionBody.contractVersion}`,
);
check(
  typeof versionBody.restApiVersion === "string" && versionBody.restApiVersion.length > 0,
  `version route reports restApiVersion ${versionBody.restApiVersion}`,
);
// The negative half: routing is real, not a handler that answers everything.
const missing = await handler(new Request("http://x/v1/nope"));
check(missing.status === 404, `an unrouted path answered ${missing.status}`);

// ---------------------------------------------------------------------------

if (failures.length > 0) {
  console.error(
    `\nnode smoke FAILED — ${failures.length} check(s):\n` +
      failures.map((line) => `  - ${line}`).join("\n"),
  );
  process.exit(1);
}
console.log("\nnode smoke passed");
