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

const server = new Server(
  { name: "agentkit-fixture", version: "1.0.0" },
  { capabilities: { tools: {} } },
);

const TOOLS: ListToolsResult["tools"] = [
  {
    name: "echo",
    description: "Echo text back",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
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
    if (typeof args.text !== "string")
      return {
        isError: true,
        content: [{ type: "text", text: "text is required" }],
      };
    return { content: [{ type: "text", text: `echo:${args.text}` }] };
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
