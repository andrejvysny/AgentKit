import type {
  AiChatMessage,
  AiProviderContinuation,
} from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import { AgentKitHostError } from "../errors.js";
import {
  providerContinuationScopesEqual,
  providerContinuationScopeMismatch,
  validateProviderContinuationScope,
} from "../continuations/validation.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { MessageRecord } from "../ports/conversation-store.js";
import type {
  ProviderContinuationScope,
  ProviderContinuationStore,
} from "../ports/provider-continuation-store.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";

export interface ProviderContinuationSession {
  readonly scope: ProviderContinuationScope;
  latest?: AiProviderContinuation;
  required: boolean;
  messages?: AiChatMessage[];
  onContinuation(state: AiProviderContinuation): Promise<void>;
}

interface PrepareInput {
  store: AssistantStore;
  ctx: TaskExecutionContext;
  client: AiProviderClient;
  providerId: string;
  model: string;
  chatId: string;
  anchorMessageId: string;
}

/** Pin credentials and branch before staging tools, hooks or provider work. */
export async function prepareProviderContinuation(
  input: PrepareInput,
): Promise<ProviderContinuationSession | undefined> {
  if (input.client.protocol !== "responses") return undefined;
  const port = input.store.continuations;
  if (!port) fail("provider_continuation_store_required");
  const records = await input.store.conversations.listMessages(input.chatId);
  const branchId = branchOf(records, input.anchorMessageId);
  const identity = input.client.connectionIdentity;
  if (!identity || identity.providerId !== input.providerId)
    fail("invalid_provider_connection");
  const scope = validateProviderContinuationScope({
    ...identity,
    chatId: input.chatId,
    branchId,
    model: input.model,
  });
  await bindScope(input, scope);
  const previous = await previousState(port, input, records);
  if (previous && !providerContinuationScopesEqual(previous.scope, scope))
    providerContinuationScopeMismatch();
  await validateAncestorState(port, records, input.ctx.task.taskId);
  const required = previous !== undefined;
  input.ctx.signal.throwIfAborted();
  const session: ProviderContinuationSession = {
    scope,
    latest: previous,
    required,
    onContinuation: async (state) => saveState(port, input, session, state),
  };
  return session;
}

async function bindScope(
  input: PrepareInput,
  scope: ProviderContinuationScope,
): Promise<void> {
  input.ctx.signal.throwIfAborted();
  await input.store.transaction(async (tx) => {
    await tx.continuations!.bindRun(
      input.ctx.task.taskId,
      scope,
      fenceOf(input.ctx),
    );
    const anchor = await tx.conversations.getMessage(input.anchorMessageId);
    await tx.conversations.updateMessage(
      input.anchorMessageId,
      {
        metadata: { ...anchor?.metadata, canonicalProviderDisplay: true },
      },
      fenceOf(input.ctx),
    );
  });
}

function branchOf(records: MessageRecord[], anchor: string): string {
  if (!records.some((record) => record.id === anchor))
    fail("provider_continuation_branch_mismatch");
  const branch =
    [...records].reverse().find((record) => record.branchIndex > 0) ??
    records[0];
  if (!branch) return fail("provider_continuation_branch_mismatch");
  return branch.id;
}

async function previousState(
  port: ProviderContinuationStore,
  input: PrepareInput,
  records: MessageRecord[],
): Promise<AiProviderContinuation | undefined> {
  const current = await port.getByRun(input.ctx.task.taskId);
  if (current) return current.state;
  const candidates = await port.findByAnchors(
    input.chatId,
    records
      .filter((record) => record.id !== input.anchorMessageId)
      .map((record) => record.id),
  );
  return candidates.at(-1)?.state;
}

async function validateAncestorState(
  port: ProviderContinuationStore,
  records: MessageRecord[],
  currentRunId: string,
): Promise<void> {
  const runIds = [
    ...new Set(
      records
        .map((record) => record.runId)
        .filter((runId): runId is string => !!runId && runId !== currentRunId),
    ),
  ];
  for (const runId of runIds) {
    await port.getByRun(runId);
  }
}

async function saveState(
  port: ProviderContinuationStore,
  input: PrepareInput,
  session: ProviderContinuationSession,
  state: AiProviderContinuation,
): Promise<void> {
  input.ctx.signal.throwIfAborted();
  if (!providerContinuationScopesEqual(session.scope, state.scope))
    providerContinuationScopeMismatch();
  await port.put(
    {
      runId: input.ctx.task.taskId,
      anchorMessageId: input.anchorMessageId,
      state,
    },
    fenceOf(input.ctx),
  );
  input.ctx.signal.throwIfAborted();
  session.latest = state;
  session.required = true;
}

export function continuationRequest(
  session?: ProviderContinuationSession,
): Pick<
  AiChatRequest,
  | "continuation"
  | "continuationScope"
  | "continuationRequired"
  | "onContinuation"
> {
  return session
    ? {
        continuation: session.latest,
        continuationScope: session.scope,
        continuationRequired: session.required,
        onContinuation: session.onContinuation,
      }
    : {};
}

export function rememberContinuationMessages(
  session: ProviderContinuationSession | undefined,
  messages: readonly AiChatMessage[],
  appended: readonly AiChatMessage[],
): void {
  if (session) session.messages = [...messages, ...appended];
}

export function continuationFollowup(
  session: ProviderContinuationSession,
  content: string,
): AiChatMessage[] {
  if (!session.messages) return fail("provider_continuation_history_missing");
  return [...session.messages, { role: "user", content }];
}

function fenceOf(ctx: TaskExecutionContext) {
  return { taskId: ctx.task.taskId, leaseToken: ctx.leaseToken };
}
function fail(code: string): never {
  throw new AgentKitHostError(code, `Provider continuation failed: ${code}`);
}
