import type { ConnectionRecord } from "@agentkit/contracts";
export type { ConnectionRecord, ConnectionStatus } from "@agentkit/contracts";
import type { AiToolEffect } from "@agentkit/contracts";
import { AiToolRegistry } from "@agentkit/core";
import type { SecretStore, ToolContributionContext } from "@agentkit/host";
import type { McpServerConfig } from "./config.js";
import { McpError } from "./errors.js";
import { McpClientManager, type McpClientManagerDeps } from "./manager.js";
import {
  createMcpToolSetContributor,
  type McpToolSource,
} from "./contributor.js";
import type { McpToolCallOutcome } from "./projection.js";

export interface ConnectorDefinition {
  id: string;
  label: string;
  transports: readonly ("http" | "stdio")[];
  capabilityDescriptions?: readonly string[];
  configurationSchema?: import("@agentkit/contracts").AiJsonSchemaObject;
  authMethods: readonly ("token" | "oauth" | "none")[];
  /** Trusted host code; remote annotations never grant access. */
  classify(
    toolName: string,
    args: Readonly<Record<string, unknown>>,
  ): AiToolEffect;
  validate?(record: ConnectionRecord): void;
}
export interface ConnectionStore {
  get(id: string): Promise<ConnectionRecord | null>;
  list(): Promise<ConnectionRecord[]>;
  /** Compare-and-swap; expectedRevision null means create only. */
  put(record: ConnectionRecord, expectedRevision: number | null): Promise<void>;
  delete(id: string): Promise<void>;
}
export interface ConnectionGrant {
  connectionId: string;
  revision: number;
  accountId: string;
  workspaceId?: string;
  tools: readonly string[];
  effects: readonly AiToolEffect[];
  resources?: readonly string[];
  prompts?: readonly string[];
  /** Optional exact argument constraints, e.g. repository/project/workspace. */
  arguments?: Readonly<Record<string, unknown>>;
}
export interface ConnectorInvocation {
  operationId: string;
  connection: Readonly<ConnectionRecord>;
  runId: string;
  actorId?: string;
  toolName: string;
  arguments: Readonly<Record<string, unknown>>;
  effect: AiToolEffect;
  signal?: AbortSignal;
}
export interface OperationReceipt {
  operationId: string;
  connectionId: string;
  revision: number;
  accountId: string;
  runId: string;
  toolName: string;
  effect: AiToolEffect;
  arguments: Readonly<Record<string, unknown>>;
  outcome: "pending" | "succeeded" | "failed" | "cancelled" | "uncertain";
  externalResult?: McpToolCallOutcome;
  errorCode?: string;
}
export interface ConnectionServiceDeps extends McpClientManagerDeps {
  store: ConnectionStore;
  connectors: readonly ConnectorDefinition[];
  /** Must review this exact immutable action. Missing authorizer refuses writes. */
  authorize?(invocation: ConnectorInvocation): Promise<boolean>;
  /** Audit persistence is awaited before reporting a result. */
  recordOperation(receipt: OperationReceipt): Promise<void>;
  revoke?(connection: ConnectionRecord): Promise<void>;
}
function fail(
  code:
    | "mcp_invalid_config"
    | "mcp_stale_connection"
    | "mcp_invocation_refused",
  message: string,
): never {
  throw new McpError(code, message);
}
function immutable<T>(value: T): T {
  const copy = structuredClone(value);
  const freeze = (v: unknown) => {
    if (v && typeof v === "object") {
      Object.values(v).forEach(freeze);
      Object.freeze(v);
    }
  };
  freeze(copy);
  return copy;
}

/** Owns lifecycle; runs capture connection identity and cannot follow account switches. */
export class ConnectionService {
  private readonly managers = new Map<
    string,
    { fingerprint: string; manager: McpClientManager }
  >();
  private readonly connectors: Map<string, ConnectorDefinition>;
  private queue: Promise<unknown> = Promise.resolve();
  private disposed = false;
  constructor(private readonly deps: ConnectionServiceDeps) {
    this.connectors = new Map(deps.connectors.map((item) => [item.id, item]));
    if (this.connectors.size !== deps.connectors.length)
      fail("mcp_invalid_config", "Duplicate connector definition.");
  }
  catalog() {
    return [...this.connectors.values()].map(
      ({
        id,
        label,
        transports,
        authMethods,
        capabilityDescriptions,
        configurationSchema,
      }) =>
        structuredClone({
          id,
          label,
          transports,
          authMethods,
          capabilityDescriptions,
          configurationSchema,
        }),
    );
  }
  list() {
    return this.deps.store.list();
  }
  get(id: string) {
    return this.deps.store.get(id);
  }
  private serialize<T>(fn: () => Promise<T>): Promise<T> {
    const next = this.queue.then(fn);
    this.queue = next.catch(() => {});
    return next;
  }
  async save(
    input: Omit<ConnectionRecord, "status" | "errorCode">,
    expectedRevision: number | null,
  ): Promise<ConnectionRecord> {
    return this.serialize(async () => {
      this.assertActive();
      const old = await this.deps.store.get(input.id);
      if (
        (old?.revision ?? null) !== expectedRevision ||
        input.revision !== (expectedRevision ?? 0) + 1
      )
        fail("mcp_stale_connection", "Connection revision changed.");
      if (!input.id || !input.accountId)
        fail(
          "mcp_invalid_config",
          "Connection and account identities are required.",
        );
      if (
        old &&
        (old.accountId !== input.accountId ||
          old.connectorId !== input.connectorId ||
          old.workspaceId !== input.workspaceId)
      )
        fail(
          "mcp_invalid_config",
          "Account and workspace identity are immutable; create a separate connection.",
        );
      const definition = this.definition(input.connectorId);
      if (!definition.transports.includes(input.config.transport.kind))
        fail("mcp_invalid_config", "Unsupported connector transport.");
      const draft = {
        ...structuredClone(input),
        status: "disconnected" as const,
      };
      definition.validate?.(draft);
      // All headers/env values are conservatively secretized, including custom names.
      draft.config = await normalizeConnectionSecrets(
        draft.id,
        draft.config,
        this.deps.secrets,
        old?.config,
      );
      await this.drop(input.id);
      await this.deps.store.put(draft, expectedRevision);
      if (draft.config.enabled !== false) await this.connectRecord(draft);
      return (await this.deps.store.get(input.id))!;
    });
  }
  connect(id: string) {
    return this.serialize(async () => {
      this.assertActive();
      await this.connectRecord(await this.require(id));
      return this.require(id);
    });
  }
  reconnect(id: string) {
    return this.serialize(async () => {
      this.assertActive();
      await this.drop(id);
      await this.connectRecord(await this.require(id));
      return this.require(id);
    });
  }
  async test(id: string) {
    await this.connect(id);
    const record = await this.require(id);
    return this.manager(record).listTools(record.config.alias);
  }
  disconnect(id: string) {
    return this.serialize(async () => {
      const record = await this.require(id);
      await this.drop(id);
      await this.deps.store.put(
        { ...record, status: "disconnected" },
        record.revision,
      );
    });
  }
  remove(id: string) {
    return this.serialize(async () => {
      const record = await this.require(id);
      await this.drop(id);
      // Local refusal takes effect even when remote revocation fails.
      await this.deps.store.delete(id);
      try {
        await this.deps.revoke?.(record);
      } finally {
        for (const ref of await this.deps.secrets.listRefs()) {
          if (
            ref.startsWith(`connections/${encodeURIComponent(id)}/`) ||
            ref.startsWith(`oauth/${encodeURIComponent(id)}/`)
          )
            await this.deps.secrets.delete(ref);
        }
      }
    });
  }
  /** Call after out-of-band durable edits; changed records invalidate active sessions. */
  reconcile() {
    return this.serialize(async () => {
      this.assertActive();
      const records = await this.deps.store.list();
      for (const [id, live] of this.managers) {
        const record = records.find((r) => r.id === id);
        if (
          !record ||
          record.config.enabled === false ||
          live.fingerprint !== this.fingerprint(record)
        )
          await this.drop(id);
      }
      for (const record of records) {
        this.definition(record.connectorId).validate?.(record);
        if (record.config.enabled !== false && record.status !== "disconnected")
          await this.connectRecord(record);
      }
    });
  }
  /** Remote context is selected and authorized with the same account pin as tools. */
  context(grantInput: ConnectionGrant) {
    const grant = immutable(grantInput);
    const selected = async () => {
      this.assertActive();
      if (!grant.effects.includes("read"))
        fail(
          "mcp_invocation_refused",
          "Reading remote context is outside the connection grant.",
        );
      const record = await this.selected(grant);
      return { record, manager: this.manager(record) };
    };
    return {
      listResources: async (options?: { signal?: AbortSignal }) => {
        const { record, manager } = await selected();
        return (
          await manager.listResources(record.config.alias, options)
        ).filter((resource) => grant.resources?.includes(resource.uri));
      },
      readResource: async (uri: string, options?: { signal?: AbortSignal }) => {
        if (!grant.resources?.includes(uri))
          fail(
            "mcp_invocation_refused",
            "Resource is outside the connection grant.",
          );
        const { record, manager } = await selected();
        return manager.readResource(record.config.alias, uri, options);
      },
      listPrompts: async (options?: { signal?: AbortSignal }) => {
        const { record, manager } = await selected();
        return (await manager.listPrompts(record.config.alias, options)).filter(
          (prompt) => grant.prompts?.includes(prompt.name),
        );
      },
      getPrompt: async (
        name: string,
        args?: Record<string, string>,
        options?: { signal?: AbortSignal },
      ) => {
        if (!grant.prompts?.includes(name))
          fail(
            "mcp_invocation_refused",
            "Prompt is outside the connection grant.",
          );
        const { record, manager } = await selected();
        return manager.getPrompt(record.config.alias, name, args, options);
      },
    };
  }
  /** Selection precedes all discovery. The grant is pinned for this run only. */
  contributor(
    selection: (
      ctx: ToolContributionContext,
    ) => Promise<readonly ConnectionGrant[]>,
    parallelism = 4,
  ) {
    const service = this;
    return {
      namespace: "mcp",
      privileged: true,
      async contribute(ctx: ToolContributionContext) {
        ctx.signal?.throwIfAborted();
        const grants = immutable(await selection(ctx));
        if (new Set(grants.map((g) => g.connectionId)).size !== grants.length)
          fail("mcp_invalid_config", "Duplicate selected connection.");
        const tools: Awaited<
          ReturnType<
            ReturnType<typeof createMcpToolSetContributor>["contribute"]
          >
        > = [];
        let index = 0;
        await Promise.all(
          Array.from(
            { length: Math.min(Math.max(1, parallelism), grants.length) },
            async () => {
              for (;;) {
                ctx.signal?.throwIfAborted();
                const grant = grants[index++];
                if (!grant) break;
                const record = await service.selected(grant);
                const manager = service.manager(record);
                const source: McpToolSource = {
                  connectAll: async () => ({
                    connected: [record.config.alias],
                    failed: [],
                    skipped: [],
                  }),
                  connectedAliases: () => [record.config.alias],
                  listTools: async () =>
                    (
                      await manager.listTools(record.config.alias, {
                        signal: ctx.signal,
                      })
                    )
                      .filter((tool) => grant.tools.includes(tool.toolName))
                      .map((tool) => ({ ...tool, effect: "write" })),
                  callTool: async (id, args, options) =>
                    service.invoke(grant, id, args ?? {}, ctx, options?.signal),
                };
                tools.push(
                  ...(await createMcpToolSetContributor(source, {
                    connectOnContribute: false,
                    authorize: async () => true,
                  }).contribute(ctx)),
                );
              }
            },
          ),
        );
        // Bridge staging detects collisions; fail here too for direct contributor consumers.
        if (new Set(tools.map((t) => t.definition.name)).size !== tools.length)
          fail(
            "mcp_invalid_config",
            "Selected connections have colliding tool aliases.",
          );
        return tools;
      },
      dispose: () => service.dispose(),
    };
  }
  private async invoke(
    grant: ConnectionGrant,
    id: string,
    args: Record<string, unknown>,
    ctx: ToolContributionContext,
    signal?: AbortSignal,
  ) {
    this.assertActive();
    signal?.throwIfAborted();
    const record = await this.selected(grant);
    const manager = this.manager(record);
    const descriptor = (
      await manager.listTools(record.config.alias, { signal })
    ).find((tool) => tool.canonicalId === id);
    if (!descriptor || !grant.tools.includes(descriptor.toolName))
      fail("mcp_invocation_refused", "Tool is outside the connection grant.");
    const registry = new AiToolRegistry();
    registry.register({
      definition: {
        name: descriptor.registryName,
        description: descriptor.description,
        capability: id,
        version: "1",
        effect: "write",
        inputSchema: descriptor.inputSchema,
      },
      execute: async () => {
        throw new Error("validation only");
      },
    });
    if (registry.validateInput(descriptor.registryName, args).length)
      fail("mcp_invocation_refused", "Invalid tool arguments.");
    const snapshot = immutable(args);
    for (const [key, value] of Object.entries(grant.arguments ?? {}))
      if (JSON.stringify(snapshot[key]) !== JSON.stringify(value))
        fail("mcp_invocation_refused", "Arguments exceed the selected scope.");
    const effect = this.definition(record.connectorId).classify(
      descriptor.toolName,
      snapshot,
    );
    if (!grant.effects.includes(effect))
      fail(
        "mcp_invocation_refused",
        "Action effect is outside the connection grant.",
      );
    const invocation: ConnectorInvocation = immutable({
      operationId: crypto.randomUUID(),
      connection: record,
      runId: ctx.runId ?? "unscoped",
      actorId: ctx.actorId,
      toolName: descriptor.toolName,
      arguments: snapshot,
      effect,
    });
    if (effect === "write") {
      let approved = false;
      try {
        approved =
          (await this.deps.authorize?.({ ...invocation, signal })) ?? false;
      } catch {
        /* Policy faults refuse without publishing private diagnostics. */
      }
      if (!approved)
        fail("mcp_invocation_refused", "Exact action approval is required.");
    }
    // Approval may await UI. Re-check identity/lifecycle after it returns.
    signal?.throwIfAborted();
    await this.selected(grant);
    const base = {
      operationId: invocation.operationId,
      connectionId: record.id,
      accountId: record.accountId,
      revision: record.revision,
      runId: invocation.runId,
      toolName: descriptor.toolName,
      effect,
      arguments: snapshot,
    };
    try {
      await this.deps.recordOperation({ ...base, outcome: "pending" });
    } catch {
      fail(
        "mcp_invocation_refused",
        "Operation receipt storage is unavailable; no request was dispatched.",
      );
    }
    // Audit persistence was asynchronous: lifecycle and cancellation may have changed.
    signal?.throwIfAborted();
    await this.selected(grant);
    let result: McpToolCallOutcome;
    try {
      result = await manager.callTool(id, snapshot, { signal });
    } catch (error) {
      const code = error instanceof McpError ? error.code : "mcp_remote_error";
      try {
        await this.deps.recordOperation({
          ...base,
          outcome:
            code === "mcp_outcome_uncertain"
              ? "uncertain"
              : signal?.aborted
                ? "cancelled"
                : "failed",
          errorCode: code,
        });
      } catch {
        this.deps.logger?.warn("connector receipt storage failed", {
          operationId: invocation.operationId,
        });
      }
      throw error;
    }
    try {
      await this.deps.recordOperation({
        ...base,
        outcome: result.isError ? "failed" : "succeeded",
        externalResult: result,
      });
    } catch {
      throw new McpError(
        "mcp_outcome_uncertain",
        "The remote call returned but its receipt could not be retained. Reconcile before retrying.",
        {
          details: {
            operationId: invocation.operationId,
            remoteReturned: true,
          },
        },
      );
    }
    return result;
  }
  async dispose() {
    return this.serialize(async () => {
      this.disposed = true;
      for (const id of [...this.managers.keys()]) await this.drop(id);
    });
  }
  private assertActive() {
    if (this.disposed)
      fail("mcp_stale_connection", "Connection service is disposed.");
  }
  private definition(id: string) {
    const item = this.connectors.get(id);
    if (!item) fail("mcp_invalid_config", "Unknown connector.");
    return item;
  }
  private async require(id: string) {
    const record = await this.deps.store.get(id);
    if (!record) fail("mcp_stale_connection", "Connection was removed.");
    return record;
  }
  private fingerprint(record: ConnectionRecord) {
    return JSON.stringify([
      record.revision,
      record.accountId,
      record.workspaceId,
      record.config,
    ]);
  }
  private async selected(grant: ConnectionGrant) {
    const record = await this.require(grant.connectionId);
    if (
      record.revision !== grant.revision ||
      record.accountId !== grant.accountId ||
      record.workspaceId !== grant.workspaceId ||
      record.status !== "connected" ||
      record.config.enabled === false
    )
      fail(
        "mcp_stale_connection",
        "Selected connection changed or is disconnected.",
      );
    return record;
  }
  private manager(record: ConnectionRecord) {
    const live = this.managers.get(record.id);
    if (!live || live.fingerprint !== this.fingerprint(record))
      fail("mcp_stale_connection", "Connection requires reconciliation.");
    return live.manager;
  }
  private async drop(id: string) {
    const live = this.managers.get(id);
    this.managers.delete(id);
    await live?.manager.dispose();
  }
  private async connectRecord(record: ConnectionRecord) {
    if (record.config.enabled === false)
      fail("mcp_stale_connection", "Connection is disabled.");
    const fingerprint = this.fingerprint(record);
    if (
      this.managers.get(record.id)?.fingerprint === fingerprint &&
      this.managers.get(record.id)!.manager.isConnected(record.config.alias)
    )
      return;
    await this.drop(record.id);
    const manager = new McpClientManager(this.deps, [record.config]);
    await this.deps.store.put(
      { ...record, status: "connecting", errorCode: undefined },
      record.revision,
    );
    try {
      await manager.connect(record.config.alias);
      this.managers.set(record.id, { fingerprint, manager });
      await this.deps.store.put(
        { ...record, status: "connected", errorCode: undefined },
        record.revision,
      );
    } catch (error) {
      await manager.dispose();
      const code =
        error instanceof McpError ? error.code : "mcp_connect_failed";
      await this.deps.store.put(
        {
          ...record,
          status:
            code === "mcp_auth_required" || code === "mcp_secret_missing"
              ? "needs_authentication"
              : "failed",
          errorCode: code,
        },
        record.revision,
      );
      throw error;
    }
  }
}

export async function normalizeConnectionSecrets(
  id: string,
  input: McpServerConfig,
  secrets: SecretStore,
  previous?: McpServerConfig,
): Promise<McpServerConfig> {
  const config = structuredClone(input);
  if (config.transport.kind === "http") {
    const url = new URL(config.transport.url);
    if (url.username || url.password || url.search || url.hash)
      fail(
        "mcp_invalid_config",
        "MCP URL must not contain credentials, query or fragment.",
      );
  }
  const values =
    config.transport.kind === "http"
      ? config.transport.headers
      : config.transport.env;
  const refs = { ...config.secretRefs };
  const sameTarget =
    previous?.transport.kind === config.transport.kind &&
    (config.transport.kind === "http"
      ? (previous.transport as { url: string }).url === config.transport.url
      : (previous.transport as { command: string }).command ===
        config.transport.command);
  const oldValues = sameTarget
    ? previous?.transport.kind === "http"
      ? previous.transport.headers
      : previous?.transport.env
    : undefined;
  for (const [key, original] of Object.entries(values ?? {})) {
    const value = original === "***" ? oldValues?.[key] : original;
    if (value === undefined)
      fail(
        "mcp_invalid_config",
        "Redacted credentials require an unchanged existing transport.",
      );
    values![key] = value;
    if (
      config.transport.kind === "http" &&
      key.toLowerCase() === "x-workspace-slug"
    )
      continue;
    if (/\$\{[^}]+\}/.test(value)) continue;
    const placeholder = `secret_${crypto.randomUUID().replaceAll("-", "")}`;
    const ref = `connections/${encodeURIComponent(id)}/${placeholder}`;
    await secrets.set(ref, value);
    refs[placeholder] = ref;
    values![key] = `\${${placeholder}}`;
  }
  return { ...config, secretRefs: refs };
}
