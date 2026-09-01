/**
 * Host record → wire DTO.
 *
 * Every projection here DROPS something, and the omissions are the contract's,
 * not this file's inventions: `packages/contracts/src/rest.ts` documents on each
 * DTO what it leaves behind and why (queue bookkeeping, lease tokens, the
 * host-shaped body of a proposal, a message's internal ordering key). Keeping
 * the projections in one module is what makes "did we leak an internal?" a
 * question one file can answer.
 *
 * They are pure and synchronous wherever the record already holds the answer.
 * {@link proposalDto} is the exception: a proposal's outcome is stored against
 * its operation id rather than on the record, so the caller reads it first and
 * passes it in — the projection still does no I/O.
 */
import type {
  AiProviderConfig,
  ApplyOutcomeDto,
  ChatDto,
  MessageDto,
  ProposalDecisionDto,
  ProposalDto,
  RunDto,
  RunStatusDto,
} from "@agentkit/contracts";
import type {
  ApplyOutcome,
  ChatRecord,
  MessageRecord,
  ProposalDecision,
  ProposalRecord,
  TaskRecord,
} from "@agentkit/host";

export function chatDto(record: ChatRecord): ChatDto {
  return {
    id: record.id,
    ...(record.title === undefined ? {} : { title: record.title }),
    createdAt: record.createdAt,
    updatedAt: record.updatedAt,
    metadata: record.metadata,
  };
}

/**
 * Drops `orderKey` (the page cursor carries it), `modelResultJson`, and `depth`.
 *
 * `content` passes through UNCHANGED, string or parts — including an image part
 * whose `source.kind` is `"ref"`. Resolving refs here would inline the host's
 * blobs into every page of every conversation for a client that usually wants a
 * thumbnail from its own endpoint; a client that understands the host's refs
 * renders them, one that does not skips the part. See `MessageDtoSchema`.
 *
 * `depth` is the one branching field the contract deliberately does not publish:
 * it is derivable from the parent chain a client already has, and a second
 * source of truth for a message's position is a second thing that can disagree
 * with the first. `parentMessageId`, `branchIndex` and `active` are all here,
 * because none of them is derivable from what a client can see.
 */
export function messageDto(record: MessageRecord): MessageDto {
  return {
    id: record.id,
    chatId: record.chatId,
    ...(record.runId === undefined ? {} : { runId: record.runId }),
    role: record.role,
    content: record.content,
    ...(record.toolCalls === undefined ? {} : { toolCalls: record.toolCalls }),
    ...(record.toolCallId === undefined
      ? {}
      : { toolCallId: record.toolCallId }),
    ...(record.parentMessageId === undefined
      ? {}
      : { parentMessageId: record.parentMessageId }),
    branchIndex: record.branchIndex,
    active: record.active,
    metadata: record.metadata,
    createdAt: record.createdAt,
  };
}

/**
 * The chat a task belongs to, from its payload — or null when the task is not a
 * conversation's.
 *
 * `chatId` lives in the payload rather than on the record because the durable
 * task table is kind-agnostic (see `TurnRunner`'s `TurnRequest`). A run route
 * asked about an indexing task therefore has no chat to report, and the caller
 * answers 404: the id names a task, but not a RUN in the sense this contract
 * publishes.
 */
export function chatIdOfTask(task: TaskRecord): string | null {
  const chatId = task.payload["chatId"];
  return typeof chatId === "string" && chatId !== "" ? chatId : null;
}

/**
 * `createdAt` is the record's `enqueuedAt`; everything the queue uses to decide
 * *when* to run the task (priority, attempts, dead-letter bookkeeping) is
 * dropped. `status` is a 1:1 rename — `TaskStatus` and `RunStatusDto` are the
 * same six states, mirrored across the package boundary on purpose.
 */
export function runDto(task: TaskRecord, chatId: string): RunDto {
  return {
    runId: task.taskId,
    chatId,
    scopeId: task.scopeId,
    status: task.status satisfies RunStatusDto,
    createdAt: task.enqueuedAt,
    ...(task.startedAt === undefined ? {} : { startedAt: task.startedAt }),
    ...(task.finishedAt === undefined ? {} : { finishedAt: task.finishedAt }),
    ...(task.error === undefined ? {} : { error: task.error }),
  };
}

function decisionDto(decision: ProposalDecision): ProposalDecisionDto {
  return {
    actor: decision.actor,
    ...(decision.decidedBy === undefined
      ? {}
      : { decidedBy: decision.decidedBy }),
    ...(decision.policyId === undefined ? {} : { policyId: decision.policyId }),
    ...(decision.reason === undefined ? {} : { reason: decision.reason }),
    decidedAt: decision.decidedAt,
  };
}

function outcomeDto(outcome: ApplyOutcome): ApplyOutcomeDto {
  return {
    status: outcome.status,
    appliedOps: outcome.appliedOps,
    failedOps: outcome.failedOps,
  };
}

/**
 * Drops `envelope`, `operations`, `revisionAtCreate` and `operationId` — the
 * write's host-shaped body and the machinery that keeps applying it safe.
 *
 * `summary` has no column behind it, so it is projected: the record's `reason`
 * when it has one (`revision_conflict`, `interrupted`, an applier's error — the
 * line a reviewer of a terminal proposal actually needs), otherwise a string
 * `summary` from the host's own envelope if it put one there. A host with
 * nothing short to say ends up with no summary, which is what the contract asks
 * for.
 */
export function proposalDto(
  record: ProposalRecord,
  outcome: ApplyOutcome | null,
): ProposalDto {
  const summary = record.reason ?? envelopeSummary(record.envelope);
  return {
    id: record.id,
    chatId: record.chatId,
    ...(record.runId === undefined ? {} : { runId: record.runId }),
    scopeKey: record.scopeKey,
    ...(record.actionId === undefined ? {} : { actionId: record.actionId }),
    toolName: record.toolName,
    kind: record.kind,
    risk: record.risk,
    status: record.status,
    ...(summary === undefined ? {} : { summary }),
    warnings: record.warnings,
    truncated: record.truncated,
    ...(record.decision === undefined
      ? {}
      : { decision: decisionDto(record.decision) }),
    ...(outcome === null ? {} : { outcome: outcomeDto(outcome) }),
    createdAt: record.createdAt,
    ...(record.decidedAt === undefined ? {} : { decidedAt: record.decidedAt }),
    ...(record.appliedAt === undefined ? {} : { appliedAt: record.appliedAt }),
  };
}

function envelopeSummary(
  envelope: Record<string, unknown>,
): string | undefined {
  const summary = envelope["summary"];
  return typeof summary === "string" ? summary : undefined;
}

/**
 * What `listProviders` publishes.
 *
 * The contract declares no provider DTO, so this projection is the transport's
 * call — and it is a NARROWING one: `apiKey` is a credential, `extraHeaders`
 * routinely carries another, and `metadata` is where a host keeps its
 * `apiKeySecretRef`. None of the three tells a client anything it can use, and
 * all three are the kind of field that leaks once and is in a log forever.
 */
export type ProviderSummaryDto = Pick<
  AiProviderConfig,
  "id" | "label" | "kind" | "baseUrl" | "defaultModel" | "enabled"
>;

export function providerSummaryDto(
  config: AiProviderConfig,
): ProviderSummaryDto {
  return {
    id: config.id,
    label: config.label,
    kind: config.kind,
    baseUrl: config.baseUrl,
    defaultModel: config.defaultModel,
    enabled: config.enabled,
  };
}
