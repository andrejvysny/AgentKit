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
 *
 * `echo` deliberately carries `annotations.readOnlyHint: true`: effect is
 * HOST-owned, so its contributed descriptor must stay `effect:"write"`
 * without the host's `readOnly` allowance no matter what this fixture
 * claims.
 */
import { Server } from "@modelcontextprotocol/server";
import type { ListToolsResult } from "@modelcontextprotocol/server";
import { StdioServerTransport } from "@modelcontextprotocol/server/stdio";
import { appendFileSync } from "node:fs";

const logPath = process.env.AGENTKIT_FIXTURE_LOG;
const tag = process.env.AGENTKIT_FIXTURE_TAG;
const wordEcho = process.env.AGENTKIT_FIXTURE_ECHO === "word";

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

server.setRequestHandler("tools/list", async () => ({ tools: TOOLS }));

server.setRequestHandler("tools/call", async (request) => {
  const name = request.params.name;
  if (logPath) appendFileSync(logPath, `${name}\n`);
  const args = (request.params.arguments ?? {}) as Record<string, unknown>;
  if (name === "echo") {
    const value = wordEcho ? args.word : args.text;
    if (typeof value !== "string")
      return {
        isError: true,
        content: [{ type: "text", text: wordEcho ? "word is required" : "text is required" }],
      };
    return { content: [{ type: "text", text: `echo:${tag ? `${tag}:` : ""}${value}` }] };
  }
  if (name === "write_note") {
    if (typeof args.note !== "string")
      return {
        isError: true,
        content: [{ type: "text", text: "note is required" }],
      };
    return { content: [{ type: "text", text: `saved:${args.note}` }] };
  }
  return { isError: true, content: [{ type: "text", text: `unknown ${name}` }] };
});

await server.connect(new StdioServerTransport());
