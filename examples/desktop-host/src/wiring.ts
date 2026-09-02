/**
 * The composition itself, factored out of `main.ts` so a test can build the
 * exact same object graph over a temp-dir sqlite file and a scripted provider,
 * ask for port 0, and drive it over real HTTP — see `../tests/smoke.test.ts`.
 *
 * Read this file as the wiring recipe every embedding follows, in order:
 *
 *  1. Ambient ports — `Clock`/`IdGenerator` (`system.ts`'s defaults) and a
 *     `SecretStore` for provider API keys.
 *  2. Storage — one `AssistantStore` behind every port (`@agentkit/adapters-sqlite`
 *     here; swap in your own backend by implementing the ports in
 *     `packages/host/src/ports/`).
 *  3. Provider config — read from the store if one exists, seeded from env on
 *     first boot otherwise.
 *  4. Tools — a `ToolSetContributor` per source (the two sample tools below;
 *     optionally an MCP bridge).
 *  5. The write pipeline — `SessionWritePolicy` + `ProposalService`. This
 *     example ships no write tools, so nothing is ever staged, but
 *     `recoverOnBoot` (step 9) needs a `ProposalService` to reconcile against.
 *  6. `SingleProcessTaskRunner` — the queue. Built before `TurnRunner` (next
 *     step), which takes it as a dependency.
 *  7. `TurnRunner` — the durable worker over `@agentkit/core`'s run loop.
 *  8. `ExecutorRegistry` + `ChatTurnExecutor`, dispatched through
 *     `createDispatchingWorker`.
 *  9. `recoverOnBoot` — clean up after the last crash, BEFORE claiming work.
 * 10. `taskRunner.startWorker(...)` — start claiming.
 * 11. `RestHandlerDeps` — what `@agentkit/transport-http` needs to serve the
 *     contract; `main.ts` hands this straight to `serveRest`.
 * 12. The optional MCP SERVER (`@agentkit/mcp-server`) over the same
 *     contributors, mounted by `main.ts` at `/mcp` — only when
 *     `AGENTKIT_MCP_SERVER_TOKEN` is set.
 */
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { MemorySecretStore } from "@agentkit/adapters-memory";
import type { AiProviderConfig } from "@agentkit/contracts";
import {
  OpenAiCompatibleClient,
  getPresetByKind,
  type AiProviderClient,
} from "@agentkit/core";
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  PROVIDER_SECRET_REF_KEY,
  ProposalService,
  SessionWritePolicy,
  TaskService,
  TurnRunner,
  type ToolGuard,
  createContributorToolCatalog,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  recoverOnBoot,
  type ApplyOutcome,
  type ApplyProposalInput,
  type Clock,
  type IdGenerator,
  type Logger,
  type ProposalApplier,
  type SecretStore,
  type ToolSetContributor,
} from "@agentkit/host";
import {
  McpClientManager,
  createMcpToolSetContributor,
} from "@agentkit/mcp-client";
import {
  createMcpServerHandler,
  createStagedToolSource,
  type McpServerHandler,
} from "@agentkit/mcp-server";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import type { RestHandlerDeps } from "@agentkit/transport-http";
import { createExampleToolSetContributor } from "./tools.js";

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

/** The header an MCP client sets to pin its session to one chat. See step 12. */
export const MCP_CHAT_HEADER = "x-agentkit-chat";

/** Where `main.ts` binds when nothing says otherwise. See {@link resolveBindHost}. */
export const DEFAULT_BIND_HOST = "127.0.0.1";

/** The addresses that are reachable only from this machine. */
const LOOPBACK_HOSTS = new Set(["127.0.0.1", "localhost", "::1", "[::1]"]);

/**
 * The address `main.ts` binds — LOOPBACK unless the operator insists otherwise.
 *
 * This matters because of what {@link buildApp} does NOT wire: there is no
 * `authenticate` and no `authorize` in the `RestHandlerDeps` below, so every
 * route on this server is open to anything that can reach the socket. That
 * includes `POST /v1/providers`, which stores provider API keys, and the chat
 * routes, which spend them. `Bun.serve` with no `hostname` binds every
 * interface, which publishes exactly that to the whole LAN — so this example
 * names the host explicitly instead of taking the default.
 *
 * `AGENTKIT_HOST` overrides it, and `loopback: false` is the caller's cue to
 * say so loudly: anything but loopback needs real `authenticate`/`authorize`
 * wired into `deps` first.
 */
export function resolveBindHost(env: WiringEnv = process.env as WiringEnv): {
  host: string;
  loopback: boolean;
} {
  const host = env.AGENTKIT_HOST?.trim() || DEFAULT_BIND_HOST;
  return { host, loopback: LOOPBACK_HOSTS.has(host) };
}

export interface BuildAppOptions {
  /** Defaults to `env.AGENTKIT_DB ?? "./agentkit.sqlite"`. */
  dbPath?: string;
  /**
   * Defaults to an `OpenAiCompatibleClient` built from the resolved provider
   * config. A test overrides this with a `MockProviderClient` so the whole
   * stack runs with no network access.
   */
  providerFactory?: (config: AiProviderConfig) => AiProviderClient;
  /** Defaults to a process-lifetime, in-memory store — see `MemorySecretStore`. */
  secrets?: SecretStore;
  clock?: Clock;
  ids?: IdGenerator;
  logger?: Logger;
  /** Defaults to `process.env`. A test passes `{}` so nothing real is read. */
  env?: WiringEnv;
}

export interface App {
  store: SqliteAssistantStore;
  dbPath: string;
  turnRunner: TurnRunner;
  taskService: TaskService;
  proposals: ProposalService;
  taskRunner: SingleProcessTaskRunner;
  mcpEnabled: boolean;
  /**
   * The MCP SERVER handler, present only when `AGENTKIT_MCP_SERVER_TOKEN` is
   * set. `main.ts` mounts it at `/mcp`; `undefined` means the route is not
   * served at all, which is the right answer for a host with no token — an
   * unauthenticated MCP endpoint is tool execution left open.
   */
  mcpServer?: McpServerHandler;
  /** What `serveRest`/`createRestHandler` need. */
  deps: RestHandlerDeps;
  /**
   * Stops the worker, disposes the tool contributors and the MCP connections
   * (if any), and closes the DB. Idempotent — a repeated signal is safe.
   */
  stop(): Promise<void>;
}

/**
 * Satisfies `ProposalService` (which `recoverOnBoot` requires) without a real
 * write tool to back it. This example contributes no write tools, so no
 * proposal is ever staged and `apply` is never actually called — a host that
 * adds a `createProposalBuilderTool` write tool (see
 * `packages/runner-local/tests/e2e-vertical-slice.test.ts` for the pattern)
 * replaces this with one that performs the write.
 */
class NoopProposalApplier implements ProposalApplier {
  async apply(_input: ApplyProposalInput): Promise<ApplyOutcome> {
    throw new Error(
      "This example ships no write tools; ProposalApplier.apply should never be reached.",
    );
  }

  async getOutcome(_operationId: string): Promise<ApplyOutcome | null> {
    return null;
  }
}

const DEFAULT_PROVIDER_ID = "default";
const DEFAULT_PROVIDER_KIND = "openai-compatible";
const API_KEY_SECRET_REF = "provider.default.apiKey";

/**
 * On first boot only: if no provider is configured yet, seed one from env
 * (falling back to the kind's preset defaults from `@agentkit/core`, then to a
 * bare `openai-compatible` localhost guess). A later boot leaves whatever is
 * already in the store alone — this is a bootstrap convenience, not a sync.
 */
async function seedProviderIfEmpty(
  store: SqliteAssistantStore,
  secrets: SecretStore,
  env: WiringEnv,
): Promise<void> {
  const existing = await store.providers.listProviders();
  if (existing.length > 0) return;

  const kind = env.AGENTKIT_PROVIDER_KIND ?? DEFAULT_PROVIDER_KIND;
  const preset = getPresetByKind(kind);
  const baseUrl =
    env.AGENTKIT_BASE_URL ??
    preset?.defaultBaseUrl ??
    "http://127.0.0.1:8000/v1";
  const defaultModel =
    env.AGENTKIT_MODEL ?? preset?.defaultModel ?? "local-model";

  const metadata: Record<string, unknown> = {};
  if (env.AGENTKIT_API_KEY) {
    // The key itself never touches `AiProviderConfig` — only its ref does.
    await secrets.set(API_KEY_SECRET_REF, env.AGENTKIT_API_KEY);
    metadata[PROVIDER_SECRET_REF_KEY] = API_KEY_SECRET_REF;
  }

  await store.providers.upsertProvider({
    id: DEFAULT_PROVIDER_ID,
    label: preset?.label ?? kind,
    kind,
    baseUrl,
    defaultModel,
    enabled: true,
    metadata,
  });
  await store.settings.updateSettings({
    defaultProviderId: DEFAULT_PROVIDER_ID,
  });
}

export async function buildApp(options: BuildAppOptions = {}): Promise<App> {
  const env = options.env ?? (process.env as WiringEnv);
  const clock = options.clock ?? defaultClock;
  const ids = options.ids ?? defaultIds;
  const secrets = options.secrets ?? new MemorySecretStore();
  const logger = options.logger;
  // (1. Ambient ports: `clock`/`ids`/`secrets` above.)

  // 2. Storage. `SqliteAssistantStore` owns its own file — no directory, no
  // schema migration, nothing else to run first.
  const dbPath = options.dbPath ?? env.AGENTKIT_DB ?? "./agentkit.sqlite";
  if (dbPath !== ":memory:") mkdirSync(dirname(dbPath), { recursive: true });
  const store = new SqliteAssistantStore(dbPath, { clock, ids });

  // 3. Provider config: seed on first boot, then always read from the store —
  // never carry the resolved config around as free-floating state.
  await seedProviderIfEmpty(store, secrets, env);

  // 4. Tools. `createExampleToolSetContributor` is this file's `example_echo` /
  // `example_now` pair; an MCP bridge is added only when configured.
  const contributors: ToolSetContributor[] = [
    createExampleToolSetContributor(clock),
  ];
  let mcp: McpClientManager | undefined;
  if (env.AGENTKIT_MCP_COMMAND) {
    mcp = new McpClientManager({ secrets, logger, clock }, [
      {
        alias: "local",
        transport: {
          kind: "stdio",
          command: env.AGENTKIT_MCP_COMMAND,
          args: (env.AGENTKIT_MCP_ARGS ?? "").split(" ").filter(Boolean),
        },
      },
    ]);
    contributors.push(createMcpToolSetContributor(mcp));
  }

  // The guard chain, defined ONCE and shared by all three consumers below.
  // Empty here — this example has no policy to enforce — but the single array
  // is the point: `TurnRunner`, `createContributorToolCatalog` and
  // `createStagedToolSource` each stage the same contributors, and a host that
  // passed guards to one of them and forgot another would advertise (or hand an
  // MCP client) a tool its own turns refuse to run.
  const toolGuards: ToolGuard[] = [];

  // 5. The write pipeline. No write tools here, so nothing is ever staged —
  // `ProposalService` exists only so `recoverOnBoot` (step 9) has one.
  const policy = new SessionWritePolicy({ clock });
  const proposals = new ProposalService({
    store,
    applier: new NoopProposalApplier(),
    policy,
    clock,
    ids,
    logger,
  });

  // 6. The queue. Built before `TurnRunner` (next), which takes it as a dep —
  // `SingleProcessTaskRunner` itself needs nothing back: the worker it will
  // dispatch to is handed to `startWorker` later (step 10), not at
  // construction.
  const taskRunner = new SingleProcessTaskRunner({ store, clock, logger });

  // 7. The durable worker over core's run loop.
  const providerFactory =
    options.providerFactory ??
    ((config: AiProviderConfig) => OpenAiCompatibleClient.fromConfig(config));
  const turnRunner = new TurnRunner({
    store,
    taskRunner,
    providerFactory,
    secrets,
    contributors,
    toolGuards,
    clock,
    ids,
    logger,
  });
  const taskService = new TaskService({ store, taskRunner, ids, clock });

  // 8. `TurnRunner` implements `TaskWorker` on its own, but this host also
  // wants `spawnChild` wired for every executor — so it dispatches through
  // the registry instead of handing `turnRunner` to `startWorker` directly.
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turnRunner));

  // 9. Clean up after the last crash BEFORE claiming anything.
  await recoverOnBoot({ taskRunner, proposals, logger });

  // 10. Start claiming.
  const handle = await taskRunner.startWorker(
    createDispatchingWorker(registry, { store, clock, logger, taskService }),
    { concurrency: 2, ownerId: "example-desktop-host" },
  );

  // 11. What the transport needs. `basePath` and `cors` are the two options
  // README.md's port checklist calls out explicitly.
  const deps: RestHandlerDeps = {
    store,
    turns: turnRunner,
    tasks: taskService,
    proposals,
    // `GET /v1/tools` answers 200 because of this line: the catalogue stages
    // the SAME contributors a turn does, so what the route advertises and what
    // a run receives cannot drift. Leave it out and the route reports 501.
    toolCatalog: createContributorToolCatalog({
      contributors,
      guards: toolGuards,
      logger,
    }),
    packages: { "@agentkit/example-desktop-host": "0.1.0-dev" },
    basePath: "/api/agentkit",
    cors: {
      origins: ["http://localhost:5173", "http://127.0.0.1:5173"],
    },
  };

  // 12. The MCP SERVER — the same tools, offered to an outside MCP client
  // (Claude Desktop, an IDE) instead of to this host's own runs. Off unless a
  // token is set: there is no unauthenticated mode, and there should not be
  // one, because what it hands out is tool execution against this host.
  //
  // `createStagedToolSource` takes the SAME `contributors` array AND the same
  // `toolGuards` the `TurnRunner` above got, so an MCP client and a chat turn
  // see one tool set under one policy.
  // `writesEnabled` is left at its `false` default — this example ships no
  // write tools anyway, and turning it on is a decision that belongs where
  // someone can see it.
  const mcpServerToken = env.AGENTKIT_MCP_SERVER_TOKEN?.trim();
  const mcpServer = mcpServerToken
    ? createMcpServerHandler({
        tools: createStagedToolSource({
          contributors,
          guards: toolGuards,
          clock,
          ids,
          ...(logger === undefined ? {} : { logger }),
        }),
        auth: { bearerToken: mcpServerToken },
        serverInfo: { name: "agentkit-example-desktop-host", version: "0.1.0" },
        // Resolved ONCE per session, from the initialize request's headers —
        // never from a message body and never from the client's announced name.
        sessionScope: (headers) => {
          const chatId = headers.get(MCP_CHAT_HEADER);
          return chatId === null ? {} : { chatId };
        },
        ...(logger === undefined ? {} : { logger }),
      })
    : undefined;

  return {
    store,
    dbPath,
    turnRunner,
    taskService,
    proposals,
    taskRunner,
    mcpEnabled: mcp !== undefined,
    ...(mcpServer === undefined ? {} : { mcpServer }),
    deps,
    async stop(): Promise<void> {
      await handle.stop();
      if (mcpServer) await mcpServer.dispose();
      // Contributors first, then the transports they were built over: a
      // contributor's `dispose` may still need the connection `mcp.dispose()`
      // is about to tear down. Both are idempotent, so a second SIGINT is safe.
      await turnRunner.disposeContributors();
      if (mcp) await mcp.dispose();
      store.close();
    },
  };
}
