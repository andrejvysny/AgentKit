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
 * Checks, end-to-end rather than smoke-in-name-only where a package has
 * something end-to-end to run:
 *
 *   1. Ajv (Node's, not Bun's) compiles `AiRunEventSchema` out of the contracts
 *      dist and validates a committed golden trace against it — the wire
 *      contract, exercised by the validator a real consumer would use.
 *   1b. The client dist constructs against a dummy URL and answers for every
 *      route in the contract's table — NO NETWORK: what would break under Node
 *      is the module graph (a `node:` import that slipped into a package meant
 *      to run in a browser too) and the `REST_ROUTES` value import, both of
 *      which a bare construction exercises.
 *   1c. The react dist LOADS under plain Node and its hooks are all there —
 *      no render, because what breaks a hooks package under Node is the module
 *      (an emitted JSX-runtime import, a leaked `react-dom` specifier, a
 *      browser global read at module scope), not the render. Its dependency-free
 *      change emitter is exercised for real.
 *   2. `runChat` from the core dist drives a stub provider to a terminal
 *      `completed`, stamping its events with core's own `createEventStamper`.
 *   3. The testing dist loads and a golden trace round-trips through it —
 *      this is the JSON-import path that needs `with { type: "json" }" to
 *      even load under Node (see `packages/testing/src/golden/golden.ts`).
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
const CLIENT_ENTRY = publishedEntry("client");
const REACT_ENTRY = publishedEntry("react");
const CORE_ENTRY = publishedEntry("core");
const TESTING_ENTRY = publishedEntry("testing");

// Apply the published mapping to every workspace specifier the dists import at
// runtime. Registered before any dynamic import below.
const WORKSPACE_ENTRIES = {
  "@agentkit/contracts": CONTRACTS_ENTRY,
  "@agentkit/client": CLIENT_ENTRY,
  "@agentkit/react": REACT_ENTRY,
  "@agentkit/core": CORE_ENTRY,
  "@agentkit/testing": TESTING_ENTRY,
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
const requireFromCore = createRequire(
  new URL("packages/core/package.json", ROOT),
);
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
check(
  Array.isArray(trace) && trace.length > 0,
  "golden trace chat-only loaded",
);

const golden = trace[0];
const goldenValid = validateEvent(golden);
if (!goldenValid) {
  console.error(JSON.stringify(validateEvent.errors, null, 2));
}
check(
  goldenValid,
  `golden event "${golden?.type}" validates against AiRunEventSchema`,
);
// The negative half: a validator that accepts anything proves nothing.
check(
  validateEvent({ type: "run.exploded", runId: "r", seq: 0 }) === false,
  "AiRunEventSchema rejects an unknown event type",
);

// ---------------------------------------------------------------------------
// 1b. Client dist: constructs against a dummy URL and covers the route table
// ---------------------------------------------------------------------------

const clientPkg = await import(CLIENT_ENTRY);
console.log("client dist");
check(
  typeof clientPkg.createAgentKitClient === "function",
  "exports createAgentKitClient",
);
check(typeof clientPkg.runPhase === "function", "exports runPhase");
check(
  typeof clientPkg.AgentKitClientError === "function",
  "exports AgentKitClientError",
);

// A port nothing listens on, and nothing is called: constructing is the check.
const restClient = clientPkg.createAgentKitClient({
  baseUrl: "http://127.0.0.1:1/api/",
});
check(
  restClient.baseUrl === "http://127.0.0.1:1/api",
  `base URL normalises to ${restClient.baseUrl}`,
);
const restOperations = Object.keys(contracts.REST_ROUTES);
const missingOperations = restOperations.filter(
  (operation) => typeof restClient[operation] !== "function",
);
check(
  missingOperations.length === 0,
  `client answers for all ${restOperations.length} contract routes` +
    (missingOperations.length === 0
      ? ""
      : ` (missing: ${missingOperations.join(", ")})`),
);
check(
  typeof restClient.drainRun === "function",
  "exports drainRun alongside the route methods",
);
// The derivation, run for real — it is pure, so Node can execute it here.
check(
  clientPkg.runPhase({
    status: "running",
    events: [{ type: "run.started" }],
  }) === "streaming" &&
    clientPkg.runPhase({ status: "queued" }) === "queued" &&
    clientPkg.runPhase({
      status: "running",
      events: [
        {
          type: "run.completed",
          data: { iterations: 1, finishReason: "stop" },
        },
      ],
    }) === "settling" &&
    clientPkg.runPhase({
      status: "completed",
      events: [
        {
          type: "run.completed",
          data: { iterations: 1, finishReason: "stop" },
        },
      ],
    }) === "completed",
  "runPhase waits for authoritative host settlement after provider completion",
);
check(
  clientPkg.isTerminalRunEvent({ type: "run.failed" }) === true &&
    clientPkg.isTerminalRunEvent({ type: "run.verification" }) === false,
  "isTerminalRunEvent answers through the dist",
);

// ---------------------------------------------------------------------------
// 1c. React dist: LOADS under plain Node, and that is the whole check
//
//     Not a render. There is no DOM here and no react-dom installed for this
//     script, and neither is the failure this guards against: what breaks a
//     hooks package under Node is the MODULE, not the render — a JSX runtime
//     the build emitted an import for, a `react-dom` specifier that leaked into
//     src, a browser global read at module scope. All three are import-time,
//     and all three would pass `bun test` and fail the first `next build`.
// ---------------------------------------------------------------------------

const reactPkg = await import(REACT_ENTRY);
console.log("react dist");
check(
  typeof reactPkg.AgentKitProvider === "function",
  "exports AgentKitProvider",
);
for (const hook of [
  "useChat",
  "useRun",
  "useBranches",
  "useProposals",
  "useProviders",
  "useAgentKitClient",
]) {
  check(typeof reactPkg[hook] === "function", `exports ${hook}`);
}
// The emitter is the one piece with no React in it, so it is the one piece
// this script can actually EXERCISE rather than merely load.
const emitter = reactPkg.createChangeEmitter();
let emitterCalls = 0;
const unsubscribe = emitter.subscribe(reactPkg.chatTopic("c1"), () => {
  emitterCalls += 1;
});
emitter.emit(reactPkg.chatTopic("c1"));
unsubscribe();
emitter.emit(reactPkg.chatTopic("c1"));
check(emitterCalls === 1, "the change emitter delivers once and unsubscribes");

// ---------------------------------------------------------------------------
// 2. Core dist: runChat drives a stub provider to a terminal completed
// ---------------------------------------------------------------------------

const core = await import(CORE_ENTRY);
console.log("core dist");
check(typeof core.runChat === "function", "exports runChat");
check(typeof core.AiToolRegistry === "function", "exports AiToolRegistry");
check(
  typeof core.createEventStamper === "function",
  "exports createEventStamper",
);

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
check(
  result?.terminal === "completed",
  `runChat terminal is "${result?.terminal}"`,
);
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
// 3. Testing dist: a golden trace loads and validates — this is the exact
//    path that broke before `golden.ts`'s JSON imports carried
//    `with { type: "json" }`. Node throws ERR_IMPORT_ATTRIBUTE_MISSING on a
//    bare JSON import; bun does not need the attribute, so `bun test` alone
//    could never have caught this dist being unloadable under plain Node.
// ---------------------------------------------------------------------------

const testing = await import(TESTING_ENTRY);
console.log("testing dist");
check(
  Array.isArray(testing.GOLDEN_TRACE_NAMES) &&
    testing.GOLDEN_TRACE_NAMES.includes("chat-only"),
  "exports GOLDEN_TRACE_NAMES",
);
const loadedTrace = testing.loadGoldenTrace("chat-only");
check(
  Array.isArray(loadedTrace) && loadedTrace.length > 0,
  `loadGoldenTrace("chat-only") returned ${loadedTrace?.length ?? 0} event(s)`,
);
check(
  testing.makeUserMessage("hi").role === "user",
  'makeUserMessage builds a "user" message',
);

// ---------------------------------------------------------------------------

if (failures.length > 0) {
  console.error(
    `\nnode smoke FAILED — ${failures.length} check(s):\n` +
      failures.map((line) => `  - ${line}`).join("\n"),
  );
  process.exit(1);
}
console.log("\nnode smoke passed");
