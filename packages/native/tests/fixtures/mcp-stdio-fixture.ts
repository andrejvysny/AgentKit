/**
 * Stdio MCP server fixture for the native client tests: a real remote MCP
 * server built on the official SDK SERVER package (`@modelcontextprotocol
 * /server` + its stdio transport), spawned by the Rust crate's stdio
 * connector exactly like a production third-party server.
 *
 * Every `tools/call` appends its tool name to the file named by
 * `AGENTKIT_FIXTURE_LOG` — the tests use that as the observable "did this
 * actually reach the remote" witness (local refusals must leave it
 * untouched).
 *
 * Two switches let one alias stand for two DIFFERENT server generations (the
 * registration/generation tests need callbacks that are distinguishable while
 * keeping the same tool name):
 * - `AGENTKIT_FIXTURE_TAG` prefixes `echo`'s reply (`echo:<tag>:<text>`), so a
 *   result names the process that produced it. Unset keeps the byte-exact
 *   legacy reply `echo:<text>`.
 * - `AGENTKIT_FIXTURE_ECHO=word` makes `echo` require `word` instead of `text`
 *   — same advertised name, incompatible contract.
 * - `AGENTKIT_FIXTURE_MODE_FILE` is read ONCE AT STARTUP: its content acts as
 *   both the tag and, when it is `word`, the incompatible-echo switch — so a
 *   test can flip the catalogue an automatic RECONNECT spawns by rewriting
 *   the file before the replacement child starts (env is frozen per config).
 * - `AGENTKIT_FIXTURE_INIT_DELAY_MS` + `AGENTKIT_FIXTURE_INIT_ARM_FILE`: when
 *   set and the arm file exists at startup, session initialization (the
 *   child's whole startup before it serves stdio) is delayed by that many
 *   milliseconds — the slow-initialize server the cancellation test needs.
 * - `AGENTKIT_FIXTURE_AMBIGUOUS=1` advertises an EXTRA tool literally named
 *   `mcp__names__echo` beside `echo`: under alias `names` its descriptor name
 *   is exactly `echo`'s advertised name (cross-namespace ambiguity).
 * - The tool `die` logs its call and exits the process WITHOUT answering —
 *   the induced transport death the generation/cancellation tests crash
 *   through.
 * - `AGENTKIT_FIXTURE_SLOW_CALL_MS` stalls every `tools/call` handler that
 *   many milliseconds BEFORE the marker is written: the window in which an
 *   aborted client call must be torn down while the effect is still pending.
 *
 * `echo` deliberately carries `annotations.readOnlyHint: true`: effect is
 * HOST-owned, so its contributed descriptor must stay `effect:"write"`
 * without the host's `readOnly` allowance no matter what this fixture
 * claims.
 */
import { Server } from "@modelcontextprotocol/server";
import type { ListToolsResult } from "@modelcontextprotocol/server";
import { StdioServerTransport } from "@modelcontextprotocol/server/stdio";
import { appendFileSync, existsSync, readFileSync } from "node:fs";

const logPath = process.env.AGENTKIT_FIXTURE_LOG;
const mode = process.env.AGENTKIT_FIXTURE_MODE_FILE
  ? (readFileSync(process.env.AGENTKIT_FIXTURE_MODE_FILE, "utf8") || "").trim()
  : "";
const tag = process.env.AGENTKIT_FIXTURE_TAG || mode;
const wordEcho =
  process.env.AGENTKIT_FIXTURE_ECHO === "word" || mode === "word";
const ambiguous = process.env.AGENTKIT_FIXTURE_AMBIGUOUS === "1";

// A slow-initialize server: armed by the arm file's existence, so one config
// can be fast on the first spawn and slow on the reconnect's replacement.
const initDelay = Number(process.env.AGENTKIT_FIXTURE_INIT_DELAY_MS ?? "0");
const slowInit =
  initDelay > 0 &&
  !!process.env.AGENTKIT_FIXTURE_INIT_ARM_FILE &&
  existsSync(process.env.AGENTKIT_FIXTURE_INIT_ARM_FILE);
const slowCallMs = Number(process.env.AGENTKIT_FIXTURE_SLOW_CALL_MS ?? "0");

type EchoSchema = {
  type: "object";
  properties: Record<string, { type: "string" }>;
  required: string[];
};

const ECHO_SCHEMA: EchoSchema = wordEcho
  ? {
      type: "object",
      properties: { word: { type: "string" } },
      required: ["word"],
    }
  : {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    };

const server = new Server(
  { name: "agentkit-fixture", version: "1.0.0" },
  { capabilities: { tools: {} } },
);

const TOOLS: ListToolsResult["tools"] = [
  {
    name: "echo",
    description: "Echo text back",
    inputSchema: ECHO_SCHEMA,
    annotations: { readOnlyHint: true },
  },
  {
    name: "write_note",
    description: "Persist a note",
    inputSchema: {
      type: "object",
      properties: { note: { type: "string" } },
      required: ["note"],
    },
  },
];

if (ambiguous) {
  // A remote name that IS another tool's advertised descriptor name under
  // alias `names` (`mcp__names__echo`): the cross-namespace ambiguity the
  // connect gate must refuse.
  TOOLS.push({
    name: "mcp__names__echo",
    description: "Foreign-named shadow",
    inputSchema: {
      type: "object",
      properties: { greeting: { type: "string" } },
      required: ["greeting"],
    },
  });
}
if (process.env.AGENTKIT_FIXTURE_DIE === "1") {
  TOOLS.push({
    name: "die",
    description: "Crash the server process (induced transport death)",
    inputSchema: { type: "object", properties: {} },
  });
}

server.setRequestHandler("tools/list", async () => ({ tools: TOOLS }));

server.setRequestHandler("tools/call", async (request) => {
  const name = request.params.name;
  if (slowCallMs > 0) {
    // A slow server-side effect: the marker below is written only after the
    // stall, so "the remote acted" stays observable against a client abort.
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, slowCallMs);
    await promise;
  }
  if (logPath) appendFileSync(logPath, `${name}\n`);
  const args = (request.params.arguments ?? {}) as Record<string, unknown>;
  if (name === "die") {
    // Exit WITHOUT answering: the pending request dies with the transport.
    process.exit(0);
  }
  if (name === "mcp__names__echo") {
    if (typeof args.greeting !== "string")
      return {
        isError: true,
        content: [{ type: "text", text: "greeting is required" }],
      };
    return { content: [{ type: "text", text: `shadow:${args.greeting}` }] };
  }
  if (name === "echo") {
    const value = wordEcho ? args.word : args.text;
    if (typeof value !== "string")
      return {
        isError: true,
        content: [
          {
            type: "text",
            text: wordEcho ? "word is required" : "text is required",
          },
        ],
      };
    return {
      content: [{ type: "text", text: `echo:${tag ? `${tag}:` : ""}${value}` }],
    };
  }
  if (name === "write_note") {
    if (typeof args.note !== "string")
      return {
        isError: true,
        content: [{ type: "text", text: "note is required" }],
      };
    return { content: [{ type: "text", text: `saved:${args.note}` }] };
  }
  return {
    isError: true,
    content: [{ type: "text", text: `unknown ${name}` }],
  };
});

// Slow-initialize server: the child starts serving stdio only after the
// delay, so session initialization (and any request queued behind it) waits.
if (slowInit) {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, initDelay);
  await promise;
}
await server.connect(new StdioServerTransport());
