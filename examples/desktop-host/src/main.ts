#!/usr/bin/env bun
/**
 * `examples/desktop-host` — a runnable composition root.
 *
 * The whole backend is Rust: `@agentkit/native` runs the store, the durable
 * queue and worker, the governance authority, the REST v1 + SSE engine, and
 * the MCP server. This file is the thin entry point that turns the object
 * graph from `./wiring.ts` (`buildApp`) into a listening socket: one
 * `Bun.serve` on the operator-facing host/port, in front of the private
 * loopback Rust listeners, using the façade's `fetch` exactly as the
 * TypeScript transport's `fetch` was used before.
 *
 * Run it:
 *   bun run start                 # from this directory
 *   bun examples/desktop-host/src/main.ts   # from the repo root
 *
 * Env vars (all optional — see ./README.md for the full list and a curl
 * walkthrough):
 *   AGENTKIT_DB             native store file (default "./agentkit.sqlite")
 *   AGENTKIT_HOST           bind address (default "127.0.0.1" — see below)
 *   AGENTKIT_PORT           HTTP port (default 8787)
 *   AGENTKIT_PROVIDER_KIND  provider preset kind (default "openai-compatible")
 *   AGENTKIT_BASE_URL       provider base URL (default: the kind's preset)
 *   AGENTKIT_MODEL          model id (default: the kind's preset)
 *   AGENTKIT_API_KEY        provider API key, if the kind needs one
 *   AGENTKIT_MCP_COMMAND    stdio MCP server to bridge in, if set
 *   AGENTKIT_MCP_ARGS       space-separated args for that command
 *   AGENTKIT_MCP_SERVER_TOKEN  when set, serve THIS host's tools as an MCP
 *                              server (mounted at /mcp by the façade),
 *                              behind that bearer token
 *
 * No provider env is required to boot: a provider is always seeded (from
 * presets when nothing is configured), and `GET /v1/version` never touches it.
 * Submitting a chat message against an unreachable provider fails that one
 * run, not the server.
 */
import type { Logger } from "@agentkit/host";
import { buildApp, resolveBindHost, MCP_PATH } from "./wiring.js";

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

/**
 * LOOPBACK ON PURPOSE. `Bun.serve` with no `hostname` binds every interface,
 * and `buildApp` wires no client-facing `authenticate` — the façade holds the
 * private bearer token toward the Rust backend, so the default would publish
 * an unauthenticated API that stores provider API keys and spends them (the
 * chat routes) to the whole network. Naming the host is the difference
 * between a desktop backend and an open credential store.
 *
 * `AGENTKIT_HOST` overrides it. Only ever set it TOGETHER WITH a real
 * authentication layer in front of the façade in `./wiring.ts` — the warning
 * below says so at boot because a misconfiguration here is silent otherwise.
 */
const { host: HOST, loopback } = resolveBindHost();

const app = await buildApp({ logger: consoleLogger });

/**
 * The façade owns routing: `/mcp` (when an MCP server was built), the JS-
 * served catalogue routes, and the proxy to the Rust REST v1 + SSE engine —
 * see the module header of `./wiring.ts`. One socket, same paths as before.
 */
const server = Bun.serve({
  hostname: HOST,
  port: PORT,
  fetch: app.fetch,
});

if (!loopback) {
  console.warn(
    `[agentkit] WARNING: bound to ${HOST}, which is not loopback. This example wires ` +
      "no client-facing authenticate, so every route — provider API keys included — is open " +
      "to anything that can reach this socket. Wire auth in front of the façade or unset AGENTKIT_HOST.",
  );
}

console.log(
  `[agentkit] listening on http://${HOST}:${server.port}${app.basePath}`,
);
console.log(`[agentkit] db: ${app.dbPath}`);
console.log(
  `[agentkit] mcp bridge: ${app.mcpEnabled ? "enabled" : "disabled"}`,
);
console.log(
  app.mcpServer
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
