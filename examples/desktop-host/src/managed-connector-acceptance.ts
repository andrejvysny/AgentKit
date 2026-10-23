import { isDeepStrictEqual } from "node:util";
import type { AiToolExecutionContext } from "@agentkit/core";
import type { AiToolResult, ConnectionRecord } from "@agentkit/contracts";
import type { ToolContributionContext } from "@agentkit/host";
import {
  ConnectionService,
  McpError,
  referenceConnectors,
  type ConnectionGrant,
  type ConnectionServiceDeps,
  type ConnectorInvocation,
  type McpToolCallOutcome,
  type OperationReceipt,
} from "@agentkit/mcp-client";

export interface AcceptanceTarget {
  connection: Omit<ConnectionRecord, "status" | "errorCode">;
  tools: readonly string[];
  /** Exact repository or project arguments required on every ordinary call. */
  scope: Readonly<Record<string, string>>;
  identityProbe: { toolName: string; arguments: Record<string, unknown> };
  /** Verify the remote account and workspace, not merely a successful response. */
  verifyIdentity(result: AiToolResult): Promise<boolean>;
  /** Set only for a target designated by its owner for disposable test writes. */
  disposable?: true;
}

export interface AcceptanceEvidence {
  schemaVersion: 1;
  execution: "live" | "deterministic";
  kind: "identity" | "discovery" | "operation";
  connectionId: string;
  accountId: string;
  workspaceId?: string;
  revision: number;
  operationId?: string;
  runId?: string;
  toolName?: string;
  effect?: "read" | "write";
  outcome?: OperationReceipt["outcome"];
  errorCode?: string;
  toolCount?: number;
  /** Extract only public remote IDs. No raw content, arguments, URLs or tokens. */
  externalIds?: readonly string[];
}

export interface AcceptanceOptions {
  deps: Omit<
    ConnectionServiceDeps,
    "connectors" | "authorize" | "recordOperation"
  >;
  targets: readonly AcceptanceTarget[];
  execution: AcceptanceEvidence["execution"];
  evidence(event: AcceptanceEvidence): Promise<void>;
  /** Required durable ledger. Unlike evidence, this contains exact action arguments. */
  recordOperation(receipt: OperationReceipt): Promise<void>;
  /** Absent by default. Must approve the exact validated invocation each time. */
  approveExactAction?(invocation: ConnectorInvocation): Promise<boolean>;
  /** Trusted host extraction; never copy remote text or arbitrary result fields. */
  externalIds?(result: McpToolCallOutcome): readonly string[];
}

function refused(message: string): never {
  throw new McpError("mcp_invocation_refused", message);
}

function pinned(
  record: ConnectionRecord,
  tools: readonly string[],
): ConnectionGrant {
  return {
    connectionId: record.id,
    accountId: record.accountId,
    workspaceId: record.workspaceId,
    revision: record.revision,
    tools,
    effects: ["read"],
  };
}

function validateTarget(target: AcceptanceTarget): void {
  const keys =
    target.connection.connectorId === "github"
      ? ["owner", "repo"]
      : target.connection.connectorId === "plane"
        ? ["project_id"]
        : [];
  if (!keys.length || keys.some((key) => !target.scope[key]?.trim()))
    refused(
      "Acceptance requires an exact GitHub owner/repository or Plane project scope.",
    );
}

/** Opt-in acceptance driver over the production service; never another agent loop. */
export async function createManagedConnectorAcceptance(
  options: AcceptanceOptions,
) {
  options.targets.forEach(validateTarget);
  const targets = structuredClone(
    options.targets.map(({ verifyIdentity, ...target }) => target),
  );
  const targetsById = new Map(
    targets.map((target, index) => [
      target.connection.id,
      {
        ...target,
        verifyIdentity: options.targets[index]!.verifyIdentity,
      },
    ]),
  );
  if (targetsById.size !== targets.length)
    refused("Duplicate acceptance target.");
  const identityEvidence = (record: ConnectionRecord) => ({
    schemaVersion: 1 as const,
    execution: options.execution,
    connectionId: record.id,
    accountId: record.accountId,
    workspaceId: record.workspaceId,
    revision: record.revision,
  });
  const service = new ConnectionService({
    ...options.deps,
    connectors: referenceConnectors,
    authorize: async (invocation) => {
      const target = targetsById.get(invocation.connection.id);
      if (!target?.disposable || !Object.keys(target.scope).length)
        return false;
      if (
        !Object.entries(target.scope).every(
          ([key, value]) => invocation.arguments[key] === value,
        )
      )
        return false;
      return (await options.approveExactAction?.(invocation)) === true;
    },
    recordOperation: async (receipt) => {
      await options.recordOperation(receipt);
      const record = await options.deps.store.get(receipt.connectionId);
      const target = targetsById.get(receipt.connectionId)!;
      await options.evidence({
        ...identityEvidence(
          record ?? { ...target.connection, status: "disconnected" },
        ),
        accountId: receipt.accountId,
        revision: receipt.revision,
        kind: "operation",
        operationId: receipt.operationId,
        runId: receipt.runId,
        toolName: receipt.toolName,
        effect: receipt.effect,
        outcome: receipt.outcome,
        ...(receipt.errorCode ? { errorCode: receipt.errorCode } : {}),
        ...(receipt.externalResult && options.externalIds
          ? { externalIds: [...options.externalIds(receipt.externalResult)] }
          : {}),
      });
    },
  });
  try {
    for (const target of targetsById.values()) {
      const existing = await service.get(target.connection.id);
      if (!existing) await service.save(target.connection, null);
      else if (
        !isDeepStrictEqual(
          { ...existing, status: undefined, errorCode: undefined },
          { ...target.connection, status: undefined, errorCode: undefined },
        )
      )
        refused(
          "Existing acceptance configuration differs; update it explicitly.",
        );
      else await service.connect(existing.id);
    }
  } catch (error) {
    await service.dispose();
    throw error;
  }

  async function select(
    connectionIds: readonly string[],
    inputContext: ToolContributionContext,
  ) {
    const context = {
      ...inputContext,
      bindings: structuredClone(inputContext.bindings),
      limits: structuredClone(inputContext.limits),
    };
    if (!context.runId) refused("Acceptance runs require an explicit run ID.");
    if (new Set(connectionIds).size !== connectionIds.length)
      refused("Duplicate selected target.");
    const grants: ConnectionGrant[] = [];
    const records = new Map<string, ConnectionRecord>();
    for (const id of connectionIds) {
      const target = targetsById.get(id);
      if (!target) refused("Unknown acceptance target.");
      const record = await service.get(id);
      if (!record) refused("Acceptance target was removed.");
      records.set(id, record);
      grants.push({
        ...pinned(record, target.tools),
        arguments: target.scope,
        effects:
          target.disposable && options.approveExactAction
            ? ["read", "write"]
            : ["read"],
      });
    }
    // Stage before verification so an awaited verifier cannot follow a new session.
    const tools = await service
      .contributor(async () => grants)
      .contribute(context);
    for (const record of records.values()) {
      const target = targetsById.get(record.id)!;
      const probeTools = await service
        .contributor(async () => [
          pinned(record, [target.identityProbe.toolName]),
        ])
        .contribute(context);
      const probe = probeTools[0];
      if (!probe) refused("The identity probe was not discovered.");
      const result = await probe.execute(
        { ...context, runId: context.runId },
        target.identityProbe.arguments,
      );
      if (!result.ok || !(await target.verifyIdentity(result)))
        refused(
          "Remote account or workspace identity did not match the selected target.",
        );
      await options.evidence({ ...identityEvidence(record), kind: "identity" });
      await options.evidence({
        ...identityEvidence(record),
        kind: "discovery",
        toolCount: tools.filter((tool) =>
          tool.definition.capability?.startsWith(`mcp.${record.config.alias}.`),
        ).length,
      });
    }
    return {
      tools: tools.map((tool) => tool.definition),
      async call(
        connectionId: string,
        toolName: string,
        args: Record<string, unknown>,
        signal?: AbortSignal,
      ) {
        const target = targetsById.get(connectionId);
        if (!target || !connectionIds.includes(connectionId))
          refused("Target was not selected for this run.");
        const tool = tools.find(
          (item) =>
            item.definition.capability ===
            `mcp.${records.get(connectionId)!.config.alias}.${records.get(connectionId)!.config.toolAliases?.[toolName] ?? toolName}`,
        );
        if (!tool) refused("Tool was not selected for this run.");
        const execution: AiToolExecutionContext = {
          ...context,
          runId: context.runId!,
          signal:
            signal && context.signal
              ? AbortSignal.any([signal, context.signal])
              : (signal ?? context.signal),
        };
        return tool.execute(execution, args);
      },
    };
  }
  return { service, select, dispose: () => service.dispose() };
}
