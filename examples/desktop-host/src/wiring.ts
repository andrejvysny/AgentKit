/**
 * Native embedding recipe: Rust owns profiles, selected content, sessions,
 * provider dispatch, durable work, and REST/SSE. Managed provider mode requires
 * explicit host vault and consent ports. Legacy fixed connections and custom
 * callbacks remain explicit extension lanes. The loopback facade adds tools,
 * version, MCP mounting, and the legacy provider display only.
 */
import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import type { AiProviderConfig } from "@agentkit/contracts";
import { CONTRACT_VERSION, REST_API_VERSION } from "@agentkit/contracts";
import type { AiProviderClient } from "@agentkit/core";
import type { Clock, Logger } from "@agentkit/host";
import {
  NativeSessionService,
  NativeStore,
  bindingId,
  registerMcpTools,
  type NativeHttpServerInfo,
  type NativeMcpServerInfo,
  type NativeMcpConnection,
  type ProviderSecretStore,
  type ProviderConsentHandler,
  type NativeServiceOptions,
  type ToolDefinitionJson,
} from "@agentkit/native";
import { createProviderCallback } from "./provider-callback.js";
import { createExampleTools } from "./tools.js";
import {
  resolveProvider,
  providerConfigOf,
  legacyProviderRecord,
  seedManagedProvider,
} from "./providers.js";

/** The env vars this recipe reads. All optional — see ../README.md. */
export interface WiringEnv {
  AGENTKIT_DB?: string;
  AGENTKIT_HOST?: string;
  AGENTKIT_PROVIDER_KIND?: string;
  AGENTKIT_BASE_URL?: string;
  AGENTKIT_MODEL?: string;
  AGENTKIT_API_KEY?: string;
  AGENTKIT_MCP_COMMAND?: string;
  AGENTKIT_MCP_ARGS?: string;
  AGENTKIT_MCP_SERVER_TOKEN?: string;
}

/** Where `main.ts` binds when nothing says otherwise. See {@link resolveBindHost}. */
export const DEFAULT_BIND_HOST = "127.0.0.1";

/** The addresses that are reachable only from this machine. */
const LOOPBACK_HOSTS: Record<string, true> = {
  "127.0.0.1": true,
  localhost: true,
  "::1": true,
  "[::1]": true,
};

/**
 * The address `main.ts` binds — LOOPBACK unless the operator insists otherwise.
 *
 * This matters because of what {@link buildApp} does NOT wire: no client
 * `authenticate`/`authorize` — the façade injects the private bearer token
 * toward the Rust backend, so every route it serves is open to anything that
 * can reach the socket. That includes `GET /v1/providers` and the chat
 * routes, which spend the seeded provider's API key. `Bun.serve` with no
 * `hostname` binds every interface, which publishes exactly that to the whole
 * LAN — so this example names the host explicitly instead of taking the
 * default.
 *
 * `AGENTKIT_HOST` overrides it, and `loopback: false` is the caller's cue to
 * say so loudly: anything but loopback needs real authentication in front of
 * the façade first.
 */
export function resolveBindHost(env: WiringEnv = process.env as WiringEnv): {
  host: string;
  loopback: boolean;
} {
  const host = env.AGENTKIT_HOST?.trim() || DEFAULT_BIND_HOST;
  return { host, loopback: host in LOOPBACK_HOSTS };
}

/** Where `main.ts` (and the façade) mount the MCP server, when one is built. */
export const MCP_PATH = "/mcp";

/** The exact `deps.cors.origins` the TypeScript transport was configured with
 * before the migration — the two Vite dev-server origins. */
const CORS_ORIGINS: Record<string, true> = {
  "http://localhost:5173": true,
  "http://127.0.0.1:5173": true,
};

/** The route base the old `RestHandlerDeps.basePath` carried; unchanged. */
export const BASE_PATH = "/api/agentkit";

export interface BuildAppOptions {
  /** Defaults to `env.AGENTKIT_DB ?? "./agentkit.sqlite"`; `":memory:"`
   * selects a native in-memory store. */
  dbPath?: string;
  /**
   * Hands the composition a custom `AiProviderClient` through the explicit JS
   * callback extension. Absent, providerMode selects native managed or legacy
   * fixed-connection dispatch.
   */
  providerFactory?: (config: AiProviderConfig) => AiProviderClient;
  /** Managed is the default. Legacy explicitly selects process-local fixed connections. */
  providerMode?: "managed" | "legacy";
  /** Required in managed mode; the host owns durable storage and consent UI. */
  providerPorts?: {
    secrets: ProviderSecretStore;
    consent: ProviderConsentHandler;
  };
  /** Host-selected connector grants; native content assembly remains authoritative. */
  contentAuthority?: {
    authorize: NonNullable<
      Parameters<NativeSessionService["setContentAuthority"]>[0]
    >;
    identity?: NonNullable<
      Parameters<NativeSessionService["setContentAuthority"]>[1]
    >;
  };
  /** Defaults to `new Date()`'s ISO clock — see `packages/host/src/ports/system.ts`. */
  clock?: Clock;
  logger?: Logger;
  /** Defaults to `process.env`. A test passes `{}` so nothing real is read. */
  env?: WiringEnv;
  /**
   * Bearer secret for the PRIVATE Rust REST listener (the façade injects it,
   * clients never see it). Defaults to 32 fresh random bytes per boot.
   */
  httpBearerToken?: string;
}

export interface App {
  store: NativeStore;
  service: NativeSessionService;
  dbPath: string;
  /** The URL prefix every REST route lives under (`{origin}{basePath}/v1/...`). */
  basePath: string;
  /** Where the private Rust REST listener answers (loopback, ephemeral port). */
  http: NativeHttpServerInfo;
  /** True when the stdio MCP bridge (`AGENTKIT_MCP_COMMAND`) connected. */
  mcpEnabled: boolean;
  /**
   * Where the Rust MCP server listens, present only when
   * `AGENTKIT_MCP_SERVER_TOKEN` is set. The façade proxies `/mcp` to it;
   * `undefined` means the route is not served at all, which is the right
   * answer for a host with no token — an unauthenticated MCP endpoint is
   * tool execution left open.
   */
  mcpServer?: NativeMcpServerInfo;
  /** The loopback façade `main.ts` hands to `Bun.serve` (see the file header). */
  fetch(request: Request): Promise<Response>;
  /**
   * Closes the session service (which stops both Rust servers first),
   * revokes the provider connection if one was registered, and releases the
   * store. Idempotent — a repeated signal is safe.
   */
  stop(): Promise<void>;
}

/** Proxies one request to a private Rust listener at `targetUrl` (already
 * carrying the upstream path and query). `bearerToken` is injected for the
 * REST server and passed through untouched (i.e. absent) for the MCP server,
 * whose token belongs to the CLIENT. Request bodies are buffered; response
 * bodies — including the SSE stream — are piped through untouched. */
async function proxy(
  targetUrl: string,
  request: Request,
  bearerToken: string | null,
  corsOrigin: string | null,
): Promise<Response> {
  const headers = new Headers(request.headers);
  headers.delete("host");
  headers.delete("connection");
  if (bearerToken !== null)
    headers.set("authorization", `Bearer ${bearerToken}`);
  const upstream = await fetch(targetUrl, {
    method: request.method,
    headers,
    ...(request.body === null ? {} : { body: await request.arrayBuffer() }),
    redirect: "manual",
  });
  const responseHeaders = new Headers(upstream.headers);
  if (corsOrigin !== null)
    responseHeaders.set("access-control-allow-origin", corsOrigin);
  return new Response(upstream.body, {
    status: upstream.status,
    headers: responseHeaders,
  });
}

function json(body: unknown, corsOrigin: string | null): Response {
  const headers = new Headers({ "content-type": "application/json" });
  if (corsOrigin !== null)
    headers.set("access-control-allow-origin", corsOrigin);
  return new Response(JSON.stringify(body), { status: 200, headers });
}

/** The transport's problem+json envelope, kept for the façade's own 404s. */
function problem(
  status: number,
  code: string,
  detail: string,
  instance: string,
): Response {
  return new Response(
    JSON.stringify({
      type: `https://agentkit.dev/problems/${code}`,
      title: status === 404 ? "Not Found" : "Error",
      status,
      code,
      detail,
      instance,
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

export async function buildApp(options: BuildAppOptions = {}): Promise<App> {
  const env = options.env ?? (process.env as WiringEnv);
  const managed = options.providerMode !== "legacy" && !options.providerFactory;
  if (options.providerMode === "managed" && options.providerFactory)
    throw new Error("Managed provider mode cannot use providerFactory.");
  if (managed && !options.providerPorts)
    throw new Error(
      "Managed provider mode requires explicit durable secret and consent ports.",
    );
  const clock = options.clock ?? {
    now: () => new Date(),
    nowIso: () => new Date().toISOString(),
  };
  const logger = options.logger;
  // 2. Storage. One native store actor thread owns the file; a busy file
  // rejects `already_owned` and there is nothing else to run first.
  const dbPath = options.dbPath ?? env.AGENTKIT_DB ?? "./agentkit.sqlite";
  if (dbPath !== ":memory:") mkdirSync(dirname(dbPath), { recursive: true });
  const store =
    dbPath === ":memory:"
      ? await NativeStore.inMemory()
      : await NativeStore.open(dbPath);

  let providerConnectionId: string | undefined;
  let serviceToClose: NativeSessionService | undefined;
  try {
    const provider = resolveProvider(env);
    const tools = createExampleTools(clock);
    let connection: NativeMcpConnection | undefined;
    if (env.AGENTKIT_MCP_COMMAND)
      connection = await store.mcpConnect({
        alias: "local",
        transport: {
          kind: "stdio",
          command: env.AGENTKIT_MCP_COMMAND,
          args: (env.AGENTKIT_MCP_ARGS ?? "").split(" ").filter(Boolean),
        },
      });
    const authority: NativeServiceOptions = {
      profiles: [
        {
          id: "desktop-host",
          extends: "automation",
          instructions:
            "Use the host-provided tools to answer the user accurately.",
          tools: [
            ...tools.map((tool) => tool.definition.name),
            ...(connection?.tools.map((tool) => tool.name) ?? []),
          ],
          resources: { projectInstructions: false },
        },
      ],
      jsToolAuthority: { configuredMode: "full_auto" },
    };
    let service: NativeSessionService;
    if (options.providerFactory) {
      const client = options.providerFactory(providerConfigOf(provider));
      service = await store.createService(authority);
      service.setProvider(createProviderCallback(client));
    } else if (managed) {
      service = await store.createService(authority);
      service.setProviderSecretStore(options.providerPorts!.secrets);
      service.setProviderConsent(options.providerPorts!.consent);
    } else {
      // The key crosses into Rust exactly once, here; the service options
      // carry only the connection id, so they can be logged freely.
      const connectionId =
        await NativeSessionService.registerProviderConnection(
          provider.baseUrl,
          provider.apiKey ?? null,
        );
      providerConnectionId = connectionId;
      service = await store.createService({
        ...authority,
        providerConfig: {
          protocol: "chat-completions",
          providerId: provider.id,
          connectionId,
          model: provider.defaultModel,
        },
      });
    }

    serviceToClose = service;
    if (options.contentAuthority)
      service.setContentAuthority(
        options.contentAuthority.authorize,
        options.contentAuthority.identity,
      );

    // 4. Tools. Every descriptor registered here is both what the Rust
    // executor can call and what the façade's `GET /v1/tools` advertises —
    // one array, so what the route shows and what a run receives cannot drift
    // (the exact invariant the old `createContributorToolCatalog` carried).
    const catalogue: ToolDefinitionJson[] = [];
    for (const tool of tools) {
      await service.registerTool(tool.definition, tool.run);
      catalogue.push(tool.definition);
    }
    let mcpEnabled = false;
    if (connection) {
      const descriptors = await registerMcpTools(service, store, connection);
      logger?.info("mcp bridge connected", {
        alias: connection.alias,
        tools: descriptors.length,
      });
      for (const descriptor of descriptors) {
        catalogue.push({
          name: descriptor.name,
          version: descriptor.version,
          effect: descriptor.effect,
          capability: descriptor.capability,
          description: descriptor.description,
          inputSchema: descriptor.inputSchema,
        });
      }
      mcpEnabled = true;
    }

    // Extension display reflects this process; managed metadata stays native.
    const providers = managed ? [] : [legacyProviderRecord(provider)];

    const bearerToken =
      options.httpBearerToken ?? randomBytes(32).toString("hex");
    const http = await service.startHttpServer({
      host: "127.0.0.1",
      port: 0,
      basePath: BASE_PATH,
      bearerToken,
      defaultModel: provider.defaultModel,
      sessionProfileId: "desktop-host",
      ...(managed
        ? {
            providerSecrets: "callback" as const,
            providerOperations: "native" as const,
          }
        : {}),
      // The same two origins the retired TypeScript transport was given:
      // the Rust engine enforces CORS itself for everything it serves, and
      // the façade adds the decoration for its own mount-path routes.
      cors: { origins: Object.keys(CORS_ORIGINS) },
    });

    if (managed)
      await seedManagedProvider(store, http.baseUrl, bearerToken, provider);

    const mcpServerToken = env.AGENTKIT_MCP_SERVER_TOKEN?.trim();
    const mcpServer = mcpServerToken
      ? await service.startMcpServer({
          host: "127.0.0.1",
          port: 0,
          bearerTokens: [mcpServerToken],
        })
      : undefined;

    // 7. The loopback façade — see the module header for what it does and why.
    const fetchHandler = async (request: Request): Promise<Response> => {
      const url = new URL(request.url);
      const path = url.pathname;

      const origin = request.headers.get("origin");
      const corsOrigin =
        origin !== null && origin in CORS_ORIGINS ? origin : null;
      if (request.method === "OPTIONS" && corsOrigin !== null) {
        return new Response(null, {
          status: 204,
          headers: {
            "access-control-allow-origin": corsOrigin,
            "access-control-allow-methods": "GET,POST,PATCH,DELETE,OPTIONS",
            "access-control-allow-headers":
              request.headers.get("access-control-request-headers") ??
              "content-type, idempotency-key, last-event-id, mcp-session-id, mcp-protocol-version",
          },
        });
      }

      if (mcpServer !== undefined && path === MCP_PATH) {
        // `/mcp` is matched BEFORE the REST routes, as before: one endpoint
        // for POST, GET and DELETE, and the client's own bearer token rides
        // through untouched — the Rust MCP server authenticates it. The
        // server's `url` is the streamable-HTTP endpoint itself; the façade
        // owns the mount path.
        return proxy(
          `${mcpServer.url}/${url.search}`,
          request,
          null,
          corsOrigin,
        );
      }
      if (!path.startsWith(BASE_PATH)) {
        return problem(
          404,
          "not_found",
          `No route for ${request.method} ${path}.`,
          path,
        );
      }
      const rest = path.slice(BASE_PATH.length);

      // Managed provider routes always reach the native metadata authority.
      if (request.method === "GET" && rest === "/v1/tools") {
        return json(catalogue, corsOrigin);
      }
      if (request.method === "GET" && rest === "/v1/version") {
        return json(
          {
            contractVersion: CONTRACT_VERSION,
            restApiVersion: REST_API_VERSION,
            packages: {
              "@agentkit/example-desktop-host": "0.1.0-dev",
              [bindingId]: "native",
            },
          },
          corsOrigin,
        );
      }
      if (!managed && request.method === "GET" && rest === "/v1/providers") {
        return json(providers, corsOrigin);
      }

      if (rest.startsWith("/v1/")) {
        // `http.baseUrl` already carries the server's base path, so the
        // forwarded path is the REST fragment, not the public one.
        return proxy(
          `${http.baseUrl}${rest}${url.search}`,
          request,
          bearerToken,
          corsOrigin,
        );
      }
      return problem(
        404,
        "not_found",
        `No route for ${request.method} ${path}.`,
        path,
      );
    };

    let stopping: Promise<void> | undefined;
    return {
      store,
      service,
      dbPath,
      basePath: BASE_PATH,
      http,
      mcpEnabled,
      ...(mcpServer === undefined ? {} : { mcpServer }),
      fetch: fetchHandler,
      async stop(): Promise<void> {
        // `close()` stops both Rust servers itself, before the runtime begins
        // Closing, and drains owned execution up to the close deadline. Both
        // are idempotent, so a second SIGINT is safe.
        stopping ??= (async () => {
          await service.close();
          if (providerConnectionId !== undefined)
            await NativeSessionService.revokeProviderConnection(
              providerConnectionId,
            );
          await store.close();
        })();
        return stopping;
      },
    };
  } catch (error) {
    await serviceToClose?.close();
    if (providerConnectionId !== undefined)
      await NativeSessionService.revokeProviderConnection(providerConnectionId);
    await store.close();
    throw error;
  }
}
