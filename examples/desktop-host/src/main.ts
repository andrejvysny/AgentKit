#!/usr/bin/env bun
/**
 * `examples/desktop-host` — a runnable composition root.
 *
 * This is the first place in this repo that wires store + task runner +
 * provider + tools + HTTP transport together outside test code. Everything
 * that decides HOW those pieces fit together lives in `./wiring.ts`
 * (`buildApp`); this file is the thin entry point that turns the resulting
 * `RestHandlerDeps` into a listening server, using `@agentkit/transport-http`'s
 * `serveRest` exactly as its README's "Wiring" section shows.
 *
 * Run it:
 *   bun run start                 # from this directory
 *   bun examples/desktop-host/src/main.ts   # from the repo root
 *
 * Env vars (all optional — see ./README.md for the full list and a curl
 * walkthrough):
 *   AGENTKIT_DB             sqlite file path (default "./agentkit.sqlite")
 *   AGENTKIT_PORT           HTTP port (default 8787)
 *   AGENTKIT_PROVIDER_KIND  provider preset kind (default "openai-compatible")
 *   AGENTKIT_BASE_URL       provider base URL (default: the kind's preset)
 *   AGENTKIT_MODEL          model id (default: the kind's preset)
 *   AGENTKIT_API_KEY        provider API key, if the kind needs one
 *   AGENTKIT_MCP_COMMAND    stdio MCP server to bridge in, if set
 *   AGENTKIT_MCP_ARGS       space-separated args for that command
 *
 * No provider env is required to boot: a provider is always seeded (from
 * presets when nothing is configured), and `GET /v1/version` never touches it.
 * Submitting a chat message against an unreachable provider fails that one
 * run, not the server.
 */
import { serveRest } from "@agentkit/transport-http";
import type { Logger } from "@agentkit/host";
import { buildApp } from "./wiring.js";

const consoleLogger: Logger = {
  debug: (message, fields) =>
    console.debug(`[agentkit] ${message}`, fields ?? ""),
  info: (message, fields) => console.log(`[agentkit] ${message}`, fields ?? ""),
  warn: (message, fields) =>
    console.warn(`[agentkit] ${message}`, fields ?? ""),
  error: (message, fields) =>
    console.error(`[agentkit] ${message}`, fields ?? ""),
};

const PORT = Number(process.env.AGENTKIT_PORT ?? 8787);

const app = await buildApp({ logger: consoleLogger });

const server = Bun.serve({ port: PORT, ...serveRest(app.deps) });

console.log(
  `[agentkit] listening on http://localhost:${server.port}${app.deps.basePath ?? ""}`,
);
console.log(`[agentkit] db: ${app.dbPath}`);
console.log(
  `[agentkit] mcp bridge: ${app.mcpEnabled ? "enabled" : "disabled"}`,
);

let shuttingDown = false;
async function shutdown(signal: string): Promise<void> {
  if (shuttingDown) return;
  shuttingDown = true;
  console.log(`[agentkit] ${signal} received, shutting down`);
  await server.stop(true);
  await app.stop();
  process.exit(0);
}
process.on("SIGINT", () => void shutdown("SIGINT"));
process.on("SIGTERM", () => void shutdown("SIGTERM"));
