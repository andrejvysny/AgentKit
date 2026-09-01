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
 *   AGENTKIT_HOST           bind address (default "127.0.0.1" — see below)
 *   AGENTKIT_PORT           HTTP port (default 8787)
 *   AGENTKIT_PROVIDER_KIND  provider preset kind (default "openai-compatible")
 *   AGENTKIT_BASE_URL       provider base URL (default: the kind's preset)
 *   AGENTKIT_MODEL          model id (default: the kind's preset)
 *   AGENTKIT_API_KEY        provider API key, if the kind needs one
 *   AGENTKIT_MCP_COMMAND    stdio MCP server to bridge in, if set
 *   AGENTKIT_MCP_ARGS       space-separated args for that command
 *   AGENTKIT_MCP_SERVER_TOKEN  when set, serve THIS host's tools as an MCP
 *                              server at /mcp, behind that bearer token
 *
 * No provider env is required to boot: a provider is always seeded (from
 * presets when nothing is configured), and `GET /v1/version` never touches it.
 * Submitting a chat message against an unreachable provider fails that one
 * run, not the server.
 */
import { serveRest } from "@agentkit/transport-http";
import type { Logger } from "@agentkit/host";
import { buildApp, resolveBindHost } from "./wiring.js";

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

/** Where the MCP server is mounted, when one is built. */
const MCP_PATH = "/mcp";

/**
 * LOOPBACK ON PURPOSE. `Bun.serve` with no `hostname` binds every interface,
 * and `buildApp` wires no `authenticate` and no `authorize` — so the default
 * would publish an unauthenticated API that stores provider API keys
 * (`POST /v1/providers`) and spends them (the chat routes) to the whole
 * network. Naming the host is the difference between a desktop backend and an
 * open credential store.
 *
 * `AGENTKIT_HOST` overrides it. Only ever set it TOGETHER WITH real
 * `authenticate`/`authorize` in `./wiring.ts` — the warning below says so at
 * boot because a misconfiguration here is silent otherwise.
 */
const { host: HOST, loopback } = resolveBindHost();

const app = await buildApp({ logger: consoleLogger });

/**
 * `/mcp` is matched BEFORE the REST handler sees the request.
 *
 * The two are different protocols on one socket: `serveRest` resolves paths
 * against `deps.basePath` (`/api/agentkit` here) and answers 404 for anything
 * outside it, so `/mcp` would never reach the MCP handler if it went second.
 * The exact-path test (rather than a prefix) is deliberate — the MCP streamable
 * transport uses one endpoint for POST, GET and DELETE, so there are no
 * subpaths to forward.
 */
const restFetch = serveRest(app.deps).fetch;
const mcpServer = app.mcpServer;

const server = Bun.serve({
  hostname: HOST,
  port: PORT,
  fetch(request) {
    if (mcpServer && new URL(request.url).pathname === MCP_PATH) {
      return mcpServer.fetch(request);
    }
    return restFetch(request);
  },
});

if (!loopback) {
  console.warn(
    `[agentkit] WARNING: bound to ${HOST}, which is not loopback. This example wires ` +
      "no authenticate/authorize, so every route — provider API keys included — is open " +
      "to anything that can reach this socket. Wire auth in wiring.ts or unset AGENTKIT_HOST.",
  );
}

console.log(
  `[agentkit] listening on http://${HOST}:${server.port}${app.deps.basePath ?? ""}`,
);
console.log(`[agentkit] db: ${app.dbPath}`);
console.log(
  `[agentkit] mcp bridge: ${app.mcpEnabled ? "enabled" : "disabled"}`,
);
console.log(
  mcpServer
    ? `[agentkit] mcp server: http://${HOST}:${server.port}${MCP_PATH} (bearer token required)`
    : "[agentkit] mcp server: disabled (set AGENTKIT_MCP_SERVER_TOKEN to enable)",
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
