/**
 * The composition itself, factored out of `main.ts` so a test can build the
 * exact same object graph over a temp-dir native store file and a scripted
 * provider, ask for port 0, and drive it over real HTTP — see
 * `../tests/smoke.test.ts`.
 *
 * NATIVE COMPOSITION. Every runtime decision — sessions, durable queue,
 * worker, governance, REST v1 + SSE, MCP — executes in Rust through
 * `@agentkit/native`. Read this file as the native embedding recipe, in
 * order:
 *
 *  1. Ambient ports — a `Clock` for the time tool (the rest of the ambient
 *     surface, ids and timestamps included, is stamped in Rust).
 *  2. Storage — one `NativeStore` (`open(path)` / `inMemory()`); its own
 *     fresh database identity, no schema migration, nothing to run first.
 *  3. Provider config — resolved from env, then ONE of the two native
 *     provider lanes: the Rust-native lane (`registerProviderConnection` +
 *     `createService({providerConfig})`, HTTP/SSE in Rust, streaming
 *     verbatim) for a real boot, or the JS callback lane
 *     (`service.setProvider`, bridged to an `@agentkit/core`
 *     `AiProviderClient`) whenever `providerFactory` hands us a client —
 *     that is how tests script the provider with no network.
 *  4. Tools — `registerTool(definition, run)` per tool (the two sample
 *     tools in `./tools.ts`); for the optional MCP bridge, `store.mcpConnect`
 *     + `registerMcpTools`, which feeds every remote descriptor through
 *     the same authority lane. The registered descriptors double as the
 *     `GET /v1/tools` catalogue the façade answers with (see step 7).
 *  5. `service.startHttpServer(...)` — the Rust REST v1 + SSE engine over
 *     THIS service and store. A message submitted via
 *     `POST /v1/chats/:chatId/messages` is durable work executed by this
 *     service's own worker; the bearer token is checked in Rust.
 *  6. The optional MCP SERVER — `service.startMcpServer(...)`, the Rust
 *     streamable-HTTP engine over the same registered JS tools, only when
 *     `AGENTKIT_MCP_SERVER_TOKEN` is set.
 *  7. The loopback façade — `app.fetch`. `main.ts` (and the smoke test)
 *     mount it with `Bun.serve`; it adds what the native listeners do not
 *     carry, nothing more: the `/mcp` mount, CORS for the two Vite origins,
 *     the three JS-served routes documented under "TS SLICES KEPT".
 *     Chat creation is proxied unchanged: the native server's
 *     `sessionProfileId: "desktop-host"` validates and creates each chat as
 *     an agent session in one durable insertion.
 *     It injects the private bearer token toward the Rust server, so the
 *     external API keeps its token-free loopback contract while the Rust
 *     backend still enforces its own auth floor.
 *
 * TS SLICES KEPT (each a real capability gap in the binding, not laziness):
 *  - `GET /v1/tools` and `GET /v1/version` are answered in JS. Evidence:
 *    `rust/crates/napi/src/http_server.rs` builds the backend with
 *    `DesktopBackend::open(...)` and never calls `with_tool_catalog`, so the
 *    native route answers 501 ("no tool catalog wired") — while the JS
 *    catalogue here is the exact descriptor list handed to `registerTool`,
 *    i.e. what the Rust engine actually runs.
 *  - `GET /v1/providers` is answered from a settings-backed seed document
 *    (first-boot seeding from env, exactly as before). Evidence:
 *    `createProvider`/`updateProvider` in `rust/crates/host/src/http/
 *    providers.rs` gate credential writes on a wired secret vault
 *    (`require_secret_port`), and the example deliberately starts the
 *    server WITHOUT the binding's `providerSecrets: "memory"` vault —
 *    that vault stores credentials in the connection repository, while
 *    THIS example's provider records live in the one settings document
 *    both lanes share, so the façade's list stays the authority
 *    (`settings.getSettings` / `settings.updateSettings` through
 *    `callStore`). The engine-level path itself is qualified in
 *    `packages/native/tests/http-server.test.ts`.
 *  - CORS is wired at BOTH layers now: the server itself starts with the
 *    same two Vite origins (`startHttpServer({ cors })` carries the
 *    transport's `CorsConfig` — `rust/crates/transport-http/src/cors.rs`,
 *    Server-option closure round), and the façade still answers preflight
 *    and decorates the responses IT produces (the mount-path routes —
 *    tools/version/providers/MCP — that never reach the Rust router).
 *  - The managed-integrations layer (prompt templates, skills, connection
 *    configuration, run-content) is the typed native binding wrapper —
 *    `NativeManagedContent` over `callStore` arms executed by
 *    `agentkit-store::managed_content` — see `./managed-integrations.ts`.
 *    The one honest gap is the MCP-OAuth lifecycle leg:
 *    `connect`/`test`/`action` reach the port surface and are refused
 *    `not_implemented`, exactly as the REST lane answers
 *    `connectionAction` with 501 (no OAuth in the Rust MCP client).
 *  - Remote MCP calls carry no run-scoped plan-mode/ask guard and no
 *    `x-agentkit-chat` session pinning (the binding's `BearerSet` never
 *    extracts a chat header — `rust/crates/napi/src/mcp_server.rs`); both
 *    are documented reductions, accepted here because no example behavior
 *    asserted either.
 */
import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import type {
  AiChatMessage,
  AiProviderConfig,
  AiProviderKind,
  AiToolDefinition,
} from "@agentkit/contracts";
import { CONTRACT_VERSION, REST_API_VERSION } from "@agentkit/contracts";
import { getPresetByKind, type AiProviderClient } from "@agentkit/core";
import type { Clock, Logger } from "@agentkit/host";
import {
  NativeSessionService,
  NativeStore,
  bindingId,
  registerMcpTools,
  type NativeHttpServerInfo,
  type NativeMcpServerInfo,
  type NativeServiceOptions,
  type ProviderFunction,
  type ProviderToolCall,
  type ToolDefinitionJson,
} from "@agentkit/native";
import { createExampleTools } from "./tools.js";

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

const DEFAULT_PROVIDER_ID = "default";
const DEFAULT_PROVIDER_KIND = "openai-compatible";

/** Where the provider seed document lives inside the shared settings
 * document's whitelisted `metadata` key. */
const PROVIDER_SEED_KEY = "exampleProviders";

export interface BuildAppOptions {
  /** Defaults to `env.AGENTKIT_DB ?? "./agentkit.sqlite"`; `":memory:"`
   * selects a native in-memory store. */
  dbPath?: string;
  /**
   * Hands the composition a scripted `AiProviderClient` and selects the JS
   * callback provider lane (`setProvider`) for it. Absent selects the
   * Rust-native provider lane: provider HTTP/SSE runs in Rust over a
   * `registerProviderConnection` handle, with no JS on the hot path.
   */
  providerFactory?: (config: AiProviderConfig) => AiProviderClient;
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

/** The provider settings resolved from env, shared by both lanes and by the
 * seed document: read once at boot, never carried as free-floating state
 * beyond the persisted seed. */
interface ResolvedProvider {
  id: string;
  kind: AiProviderKind;
  label: string;
  baseUrl: string;
  defaultModel: string;
  apiKey?: string;
}

function resolveProvider(env: WiringEnv): ResolvedProvider {
  // An operator-supplied kind; `getPresetByKind` answers `undefined` for an
  // unknown string, so that lookup is itself the validation.
  const kind = (env.AGENTKIT_PROVIDER_KIND ??
    DEFAULT_PROVIDER_KIND) as AiProviderKind;
  const preset = getPresetByKind(kind);
  return {
    id: DEFAULT_PROVIDER_ID,
    kind,
    label: preset?.label ?? kind,
    baseUrl:
      env.AGENTKIT_BASE_URL ??
      preset?.defaultBaseUrl ??
      "http://127.0.0.1:8000/v1",
    defaultModel: env.AGENTKIT_MODEL ?? preset?.defaultModel ?? "local-model",
    ...(env.AGENTKIT_API_KEY === undefined || env.AGENTKIT_API_KEY === ""
      ? {}
      : { apiKey: env.AGENTKIT_API_KEY }),
  };
}

/** The persisted provider-seed record, shaped like the `ProviderDto` the old
 * `store.providers` table served. */
interface ProviderSeed {
  id: string;
  label: string;
  kind: string;
  baseUrl: string;
  defaultModel: string;
  enabled: boolean;
  metadata: Record<string, unknown>;
}

function seedRecord(provider: ResolvedProvider): ProviderSeed {
  const metadata: Record<string, unknown> = {};
  // The key itself never lands in the document — only its presence is noted;
  // the credential lives in the provider lane (Rust connection handle or the
  // JS client), exactly as the old `PROVIDER_SECRET_REF_KEY` metadata meant.
  if (provider.apiKey !== undefined) metadata.credential = "env";
  return {
    id: provider.id,
    label: provider.label,
    kind: provider.kind,
    baseUrl: provider.baseUrl,
    defaultModel: provider.defaultModel,
    enabled: true,
    metadata,
  };
}

/**
 * On first boot only: if no provider is seeded in the settings document yet,
 * record one from env (falling back to the kind's preset defaults from
 * `@agentkit/core`, then to a bare `openai-compatible` localhost guess). A
 * later boot leaves whatever is already stored alone — this is a bootstrap
 * convenience, not a sync. Same semantics as the retired
 * `seedProviderIfEmpty`; only the storage moved to the one settings document
 * both lanes share, because the binding has no provider-write path (see the
 * module header).
 */
async function seedProviderIfEmpty(
  store: NativeStore,
  provider: ResolvedProvider,
): Promise<ProviderSeed[]> {
  const current = await store.callStore<{ metadata?: Record<string, unknown> }>(
    "settings.getSettings",
  );
  const seeded = current.metadata?.[PROVIDER_SEED_KEY];
  if (Array.isArray(seeded)) return seeded as ProviderSeed[];
  const record = seedRecord(provider);
  // Deliberately NO `defaultProviderId` patch: the binding's backend is
  // never pinned (`with_pinned_provider` is not exposed), so a default
  // provider id in the shared settings document would make every REST
  // submit fail `provider_not_bound` (turns.rs) by naming a connection the
  // host executor does not own. The host's provider lives in the lane, not
  // in the settings document.
  await store.callStore("settings.updateSettings", {
    patch: {
      metadata: { ...(current.metadata ?? {}), [PROVIDER_SEED_KEY]: [record] },
    },
  });
  return [record];
}

/**
 * The JS callback lane's bridge: one `AiProviderClient.streamChat` per
 * provider request, its event stream aggregated into the single
 * `ProviderResult` the binding's JS lane delivers per call. The binding
 * labels this lane as one-chunk (the Rust-native lane streams verbatim) —
 * it exists so a host can script the provider with zero network, which is
 * exactly what `../tests/smoke.test.ts` does.
 */
function createProviderCallback(client: AiProviderClient): ProviderFunction {
  return async (request) => {
    let content = "";
    let finishReason: string | undefined;
    const toolCalls: ProviderToolCall[] = [];
    for await (const event of client.streamChat({
      runId: request.runId,
      model: request.model,
      messages: request.messages as AiChatMessage[],
      tools: request.tools as AiToolDefinition[] | undefined,
      temperature: request.temperature ?? undefined,
      maxOutputTokens: request.maxOutputTokens ?? undefined,
    })) {
      switch (event.type) {
        case "run.message.delta":
          content += event.data.delta;
          break;
        case "run.message.completed":
          // The completed envelope is authoritative for the full text; the
          // deltas only matter when a client never stamps a completed event.
          if (event.data.content.length >= content.length)
            content = event.data.content;
          finishReason = event.data.finishReason ?? finishReason;
          break;
        case "run.tool.requested":
          toolCalls.push({
            id: event.data.toolCallId,
            name: event.data.toolName,
            arguments: parseArguments(event.data.argumentsJson),
          });
          break;
        case "run.failed":
          throw new Error(
            event.data.errorMessage ??
              `provider failed (${event.data.errorCode ?? "unknown"})`,
          );
        default:
          break;
      }
    }
    return {
      content,
      ...(toolCalls.length > 0 ? { toolCalls } : {}),
      ...(finishReason === undefined ? {} : { finishReason }),
    };
  };
}

function parseArguments(argumentsJson: string | undefined): unknown {
  if (argumentsJson === undefined || argumentsJson === "") return {};
  try {
    return JSON.parse(argumentsJson) as unknown;
  } catch {
    return argumentsJson;
  }
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

  // 3. Provider config, then the lane. The JS callback lane exists for the
  // scripted client; every real boot takes the Rust-native lane.
  const provider = resolveProvider(env);
  let providerConnectionId: string | undefined;
  const authority: NativeServiceOptions = {
    // The TypeScript composition ran this example with an EMPTY
    // `ToolGuard[]` — an allow-all authority chain — and its only
    // write-effect tools came through the MCP bridge (the contributor
    // declared them `effect: "write"`). `full_auto` is the native equivalent
    // of that chain: the Rust authority still judges every JS tool call, it
    // just admits, as before. The two example tools are `effect: "read"` and
    // unaffected by the choice.
    jsToolAuthority: { configuredMode: "full_auto" },
  };
  let service: NativeSessionService;
  if (options.providerFactory) {
    const client = options.providerFactory(providerConfigOf(provider));
    service = await store.createService(authority);
    service.setProvider(createProviderCallback(client));
  } else {
    // The key crosses into Rust exactly once, here; the service options
    // carry only the connection id, so they can be logged freely.
    const connectionId = await NativeSessionService.registerProviderConnection(
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

  // 4. Tools. Every descriptor registered here is both what the Rust
  // executor can call and what the façade's `GET /v1/tools` advertises —
  // one array, so what the route shows and what a run receives cannot drift
  // (the exact invariant the old `createContributorToolCatalog` carried).
  const catalogue: ToolDefinitionJson[] = [];
  for (const tool of createExampleTools(clock)) {
    await service.registerTool(tool.definition, tool.run);
    catalogue.push(tool.definition);
  }
  let mcpEnabled = false;
  if (env.AGENTKIT_MCP_COMMAND) {
    // The Rust `agentkit-mcp` client manager: stdio transport, resilient,
    // descriptors registered through the SAME authority lane as the sample
    // tools. `readOnly` stays at its false default — the old contributor
    // declared remote tools `effect: "write"` too.
    const connection = await store.mcpConnect({
      alias: "local",
      transport: {
        kind: "stdio",
        command: env.AGENTKIT_MCP_COMMAND,
        args: (env.AGENTKIT_MCP_ARGS ?? "").split(" ").filter(Boolean),
      },
    });
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

  // The provider seed document (first-boot only), before anything can read
  // the route.
  const providers = await seedProviderIfEmpty(store, provider);

  // 5. The Rust REST v1 + SSE server. Loopback, ephemeral, bearer-enforced
  // — and the token stays PRIVATE: the façade authenticates to it, which is
  // how the example keeps its token-free loopback client contract on top of
  // a backend that refuses to serve unauthenticated.
  const bearerToken =
    options.httpBearerToken ?? randomBytes(32).toString("hex");
  const http = await service.startHttpServer({
    host: "127.0.0.1",
    port: 0,
    basePath: BASE_PATH,
    bearerToken,
    defaultModel: provider.defaultModel,
    sessionProfileId: "desktop-host",
    // The same two origins the retired TypeScript transport was given:
    // the Rust engine enforces CORS itself for everything it serves, and
    // the façade adds the decoration for its own mount-path routes.
    cors: { origins: Object.keys(CORS_ORIGINS) },
  });

  // 6. The optional MCP SERVER — the same tools, offered to an outside MCP
  // client (Claude Desktop, an IDE) instead of to this host's own runs. Off
  // unless a token is set; there is no unauthenticated mode. `writesEnabled`
  // stays at its false default — the old staged tool source hid writes the
  // same way, and turning it on is a decision that belongs where someone can
  // see it.
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
      return proxy(`${mcpServer.url}/${url.search}`, request, null, corsOrigin);
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

    // The three JS-served routes (see "TS SLICES KEPT" in the header).
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
    if (request.method === "GET" && rest === "/v1/providers") {
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
}

function providerConfigOf(provider: ResolvedProvider): AiProviderConfig {
  return {
    id: provider.id,
    label: provider.label,
    kind: provider.kind,
    baseUrl: provider.baseUrl,
    defaultModel: provider.defaultModel,
    enabled: true,
    ...(provider.apiKey === undefined ? {} : { apiKey: provider.apiKey }),
  };
}
