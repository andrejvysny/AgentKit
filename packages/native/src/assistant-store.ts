/**
 * `NativeAssistantStore` — the `@agentkit/host` `AssistantStore` aggregate
 * served entirely by the Rust engine through the napi binding.
 *
 * Every sub-port method is a thin mapping: translate the port's parameters
 * into the binding's wire shape (`"<ns>.<method>"` + the port's parameter
 * names as input keys), call the store (or the open transaction's view), and
 * project the engine's JSON back into the port's record shape — epoch-millis
 * instants become ISO strings, `null` optional fields become absent, and
 * engine-owned bookkeeping fields the port never declared are dropped.
 *
 * Validation split follows the binding's contract: the ENGINE owns ordering,
 * tree invariants, transition legality, fencing, and duplicate-key refusal
 * (each refusal arrives as a stable coded error); this file only adds the
 * refusals the port promises that the engine cannot see (port-level input
 * checks like `invalid_append`/`invalid_cursor`, the `actionId` reservation
 * the port makes a store duty but the SQLite engine leaves to adapters, and
 * error-message classification where the engine reports a generic
 * `invalid_input` with a port-named marker).
 */
import type {
  AgentInputPatch,
  AgentInputQuery,
  AgentInputRecord,
  AgentInputStatus,
  AgentInputStore,
  AppendMessageInput,
  AppendEventsOptions,
  ApplyOutcome,
  AssistantSettings,
  AssistantStore,
  AttemptRecord,
  ChatRecord,
  ClaimNextInput,
  ClaimedTask,
  ContextSummaryRecord,
  ContextSummaryStore,
  ConversationStore,
  CreateAgentInput,
  CreateAttemptInput,
  CreateChatInput,
  CreateProposalInput,
  CreateTaskInput,
  EndAttemptInput,
  FencedWriteOptions,
  ForkChatResult,
  ImportConversationInput,
  Lease,
  ListChatsOptions,
  ListEventsOptions,
  ListMessagesOptions,
  ListProposalsOptions,
  MessageRecord,
  MessageSearchHit,
  OutboxAppendInput,
  OutboxClaimInput,
  OutboxRecord,
  OutboxStore,
  ProposalPatch,
  ProposalRecord,
  ProposalStatus,
  ProposalStore,
  ProviderContinuationRecord,
  ProviderContinuationScope,
  ProviderContinuationStore,
  ProviderStore,
  RunWriteFence,
  SearchMessagesOptions,
  SettingsStore,
  TaskPatch,
  TaskRecord,
  TaskStatus,
  TaskStore,
  ToolExecutionAdmission,
  ToolExecutionRecord,
  ToolExecutionStore,
  UpdateChatPatch,
  UpdateMessagePatch,
  UpdateProgressOptions,
} from "@agentkit/host";
import type {
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
  TaskEventEnvelope,
} from "@agentkit/contracts";
import type { NativeStore as RawNativeStore } from "../generated/index.js";
import { createNativeStorePort, NativeAgentError, type NativeStore } from "./index.js";
import type { NativeStorePort } from "./store-transaction.js";

/** The clock the conformance harness injects through `createTuned`: every
 * time-taking engine stamp (`now`) is taken from it instead of the system
 * clock, so aging/ordering tests can move time by hand. */
export interface NativeAssistantStoreClock {
  now(): Date;
}

export interface NativeAssistantStoreOptions {
  /** A ready `NativeStore` handle — the usual construction. */
  store?: NativeStore;
  /** The binding's raw `NativeStore` object, wrapped in the same
   * JSON/error contract as `NativeStore.callStore`. */
  raw?: RawNativeStore;
  /** Internal: an already-open transaction view (what `transaction()` hands
   * the callback's aggregate). */
  port?: NativeStorePort;
  /** Internal: injected clock for conformance tuning (see above). */
  clock?: NativeAssistantStoreClock;
}

type Json = Record<string, unknown>;

function record(value: unknown): Json | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Json)
    : null;
}

/** Engine `null` for an optional port field means "absent" in the port. */
function copy(value: Json, target: Json, key: string): void {
  const entry = value[key];
  if (entry !== null && entry !== undefined) target[key] = entry;
}

/** An instant the engine stores as epoch millis (or, for imported
 * conversations, opaque text) arrives as the port's ISO string. */
function iso(value: unknown): string | undefined {
  if (typeof value === "number") return new Date(value).toISOString();
  if (typeof value === "string") return value;
  return undefined;
}

/** Port instants (`Date | string`) cross the boundary as epoch millis; an
 * unparsable value is refused here with the port's `invalid_timestamp` —
 * storing it would poison every later comparison. */
function toEpoch(value: Date | string | number, field: string): number {
  const millis = value instanceof Date ? value.getTime() : new Date(value).getTime();
  if (Number.isNaN(millis)) {
    throw new NativeAgentError("invalid_timestamp", `${field} is not an instant: ${String(value)}`);
  }
  return millis;
}

function maybeEpoch(value: Date | string | number | undefined, field: string): number | undefined {
  return value === undefined ? undefined : toEpoch(value, field);
}

/** The optional `now` every time-taking binding op accepts; absent means the
 * store's own clock (untuned stores), present means the injected one. */
function withNow(input: Json, clock?: NativeAssistantStoreClock): Json {
  if (clock !== undefined) input.now = clock.now().getTime();
  return input;
}

/** Re-label a coded error the engine reported, when the port has a more
 * precise code for the same refusal. Only `invalid_input` messages are
 * consulted; every other error is re-thrown untouched. */
function reclassify(error: unknown, markers: readonly (readonly [string, string])[]): never {
  if (error instanceof NativeAgentError && error.code === "invalid_input") {
    for (const [marker, code] of markers) {
      if (error.message.includes(marker)) {
        throw new NativeAgentError(code, error.message, error.details);
      }
    }
  }
  throw error;
}

// ---------------------------------------------------------------------------
// Record projections: engine JSON → port records.
// ---------------------------------------------------------------------------

function projectChat(value: unknown): ChatRecord {
  const source = record(value);
  if (source === null) throw new NativeAgentError("internal_error", "chat record is not an object");
  const out: Json = {};
  copy(source, out, "id");
  copy(source, out, "title");
  copy(source, out, "metadata");
  copy(source, out, "archived");
  const created = iso(source["createdAt"]);
  if (created !== undefined) out["createdAt"] = created;
  const updated = iso(source["updatedAt"]);
  if (updated !== undefined) out["updatedAt"] = updated;
  return out as unknown as ChatRecord;
}

function projectMessage(value: unknown): MessageRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "message record is not an object");
  const out: Json = {};
  for (const key of [
    "id",
    "chatId",
    "runId",
    "role",
    "content",
    "orderKey",
    "toolCallId",
    "toolCalls",
    "modelResultJson",
    "parentMessageId",
    "depth",
    "branchIndex",
    "active",
    "archived",
    "metadata",
  ] as const) {
    copy(source, out, key);
  }
  const created = iso(source["createdAt"]);
  if (created !== undefined) out["createdAt"] = created;
  return out as unknown as MessageRecord;
}

function projectMessages(value: unknown): MessageRecord[] {
  return Array.isArray(value) ? value.map(projectMessage) : [];
}

function projectTask(value: unknown): TaskRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "task record is not an object");
  const out: Json = {};
  for (const key of [
    "taskId",
    "kind",
    "scopeId",
    "status",
    "priority",
    "payload",
    "parentTaskId",
    "dependsOn",
    "progress",
    "error",
    "attemptCount",
    "poisonCount",
    "deadLetterReason",
  ] as const) {
    copy(source, out, key);
  }
  for (const key of [
    "enqueuedAt",
    "availableAt",
    "startedAt",
    "finishedAt",
    "deadLetteredAt",
  ] as const) {
    const stamp = iso(source[key]);
    if (stamp !== undefined) out[key] = stamp;
  }
  return out as unknown as TaskRecord;
}

function projectAttempt(value: unknown): AttemptRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "attempt record is not an object");
  const out: Json = {};
  for (const key of [
    "attemptId",
    "taskId",
    "attemptNumber",
    "status",
    "ownerId",
    "error",
  ] as const) {
    copy(source, out, key);
  }
  const started = iso(source["startedAt"]);
  if (started !== undefined) out["startedAt"] = started;
  const ended = iso(source["endedAt"]);
  if (ended !== undefined) out["endedAt"] = ended;
  return out as unknown as AttemptRecord;
}

function projectLease(value: unknown): Lease {
  const source = record(value);
  if (source === null) throw new NativeAgentError("internal_error", "lease is not an object");
  const out: Json = {};
  for (const key of ["taskId", "attemptId", "ownerId", "leaseToken", "fencingToken"] as const) {
    copy(source, out, key);
  }
  const expires = iso(source["expiresAt"]);
  if (expires !== undefined) out["expiresAt"] = expires;
  return out as unknown as Lease;
}

function projectOutbox(value: unknown): OutboxRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "outbox record is not an object");
  const out: Json = {};
  for (const key of ["id", "topic", "runId", "payload", "attempts", "lastError"] as const) {
    copy(source, out, key);
  }
  for (const key of ["createdAt", "availableAt", "publishedAt"] as const) {
    const stamp = iso(source[key]);
    if (stamp !== undefined) out[key] = stamp;
  }
  return out as unknown as OutboxRecord;
}

function projectProposal(value: unknown): ProposalRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "proposal record is not an object");
  const out: Json = {};
  for (const key of [
    "id",
    "chatId",
    "runId",
    "scopeKey",
    "actionId",
    "toolName",
    "kind",
    "risk",
    "status",
    "envelope",
    "operations",
    "warnings",
    "truncated",
    "revisionAtCreate",
    "operationId",
    "decision",
    "reason",
  ] as const) {
    copy(source, out, key);
  }
  for (const key of ["createdAt", "decidedAt", "claimedAt", "appliedAt"] as const) {
    const stamp = iso(source[key]);
    if (stamp !== undefined) out[key] = stamp;
  }
  return out as unknown as ProposalRecord;
}

function projectOutcome(value: unknown): ApplyOutcome | null {
  let source = record(value);
  if (source === null) return null;
  // The engine hands back its receipt; the port's outcome is the verdict
  // inside it.
  if ("outcome" in source) source = record(source.outcome);
  if (source === null) return null;
  const out: Json = {};
  // The engine's receipt carries its own claim fields; the port's outcome is
  // the apply verdict plus the caller's optional result/revision strings.
  for (const key of ["status", "appliedOps", "failedOps", "resultJson", "revision"] as const) {
    copy(source, out, key);
  }
  return out as unknown as ApplyOutcome;
}

function projectInput(value: unknown): AgentInputRecord | undefined {
  const source = record(value);
  if (source === null) return undefined;
  const out: Json = {};
  for (const key of [
    "inputId",
    "sessionId",
    "kind",
    "status",
    "content",
    "contentHash",
    "order",
    "targetTaskId",
    "onSettled",
    "attemptId",
    "toolCallId",
    "expiresAt",
    "appliedTaskId",
    "appliedMessageId",
    "answerId",
    "createdAt",
    "updatedAt",
  ] as const) {
    copy(source, out, key);
  }
  return out as unknown as AgentInputRecord;
}

function projectContinuation(value: unknown): ProviderContinuationRecord {
  const source = record(value);
  if (source === null)
    throw new NativeAgentError("internal_error", "continuation record is not an object");
  const out: Json = {};
  for (const key of ["runId", "anchorMessageId", "state"] as const) {
    copy(source, out, key);
  }
  return out as unknown as ProviderContinuationRecord;
}

function projectSettings(value: unknown): AssistantSettings {
  const source = record(value) ?? {};
  const out: Json = { ...source };
  for (const [key, entry] of Object.entries(out)) {
    if (entry === null) delete out[key];
  }
  // The port's documented default: a settings pane must never read a blank
  // where "auto" belongs, and the store's document only carries what was
  // explicitly written.
  if (out["toolCalling"] === undefined) out["toolCalling"] = "auto";
  if (out["metadata"] === undefined) out["metadata"] = {};
  return out as unknown as AssistantSettings;
}

/** The port's `apply` mirror of `validateCreateAgentInput`
 * (`@agentkit/host`): the binding parses what it can, but the kind/policy
 * shape rules are the port's and must refuse with `invalid_input`. */
function validateAgentInput(input: CreateAgentInput): void {
  const bad = (message: string): never => {
    throw new NativeAgentError("invalid_input", message, { inputId: input.inputId });
  };
  for (const field of ["inputId", "sessionId", "contentHash"] as const) {
    const value: unknown = input[field];
    if (typeof value !== "string" || value === "") bad(`${field} must be a non-empty string.`);
  }
  if (!["steer", "follow_up", "human_request"].includes(input.kind))
    bad(`Unknown input kind: ${input.kind}.`);
  if (input.onSettled !== undefined) {
    if (input.kind !== "steer") bad("onSettled is only valid for steer.");
    if (input.onSettled !== "follow_up" && input.onSettled !== "cancel")
      bad(`Unknown onSettled policy: ${String(input.onSettled)}.`);
  }
  if (input.kind === "human_request") {
    for (const field of ["attemptId", "toolCallId"] as const) {
      const value = input[field];
      if (typeof value !== "string" || value === "") bad(`${field} is required for human_request.`);
    }
  } else if (
    input.attemptId !== undefined ||
    input.toolCallId !== undefined ||
    input.expiresAt !== undefined
  ) {
    bad("attemptId, toolCallId and expiresAt are only valid for human_request.");
  }
  if (input.targetTaskId !== undefined && input.targetTaskId === "")
    bad("targetTaskId must be non-empty when present.");
  if (
    input.expiresAt !== undefined &&
    (typeof input.expiresAt !== "string" || Number.isNaN(new Date(input.expiresAt).getTime()))
  )
    bad("expiresAt must be a valid ISO timestamp.");
  if (JSON.stringify(input.content) === undefined)
    bad("content must be JSON-serializable.");
}

/** Statuses that release an `actionId` reservation; every other status keeps
 * its key claimed (see `ACTION_ID_RELEASING_STATUSES` in the port). */
const RELEASED_ACTION_STATUSES: readonly ProposalStatus[] = ["rejected", "invalidated"];

/**
 * The port's `transition(…, "applied" | "failed")` on the engine's
 * receipt-bound terminal apply: the CAS guard, the outcome (the recorded
 * receipt if one exists, else the operations-shaped verdict the apply
 * produced), and `complete_proposal` landing the stamp.
 */
async function settleApply(
  call: <T,>(operation: string, input?: unknown) => Promise<T>,
  proposalId: string,
  from: readonly ProposalStatus[],
  to: "applied" | "failed",
  patch: ProposalPatch | undefined,
  clock: NativeAssistantStoreClock | undefined,
): Promise<unknown> {
  const current = record(await call<unknown>("proposals.get", { proposalId }));
  if (current === null)
    throw new NativeAgentError("not_found", `Proposal not found: ${proposalId}`, { proposalId });
  const status = current["status"] as ProposalStatus | undefined;
  const refuse = (why: string): never => {
    throw new NativeAgentError("invalid_proposal_transition", why, {
      proposalId,
      current: status,
      from,
      to,
    });
  };
  if (status === undefined || !from.includes(status))
    refuse(`Proposal ${proposalId} is ${String(status)}, expected one of [${from.join(", ")}].`);
  if (status !== "applying")
    refuse(`Proposal ${proposalId} is ${String(status)}; apply needs an applying proposal.`);
  const operationId = current["operationId"];
  if (typeof operationId !== "string")
    refuse(`Proposal ${proposalId} has no apply-claim operation id.`);
  const receipt = record(await call<unknown>("proposals.getOutcome", { operationId }));
  const recorded = receipt !== null && "outcome" in receipt ? record(receipt.outcome) : null;
  if (recorded === null) {
    // No outcome was ever recorded: this is the port's plain status CAS,
    // exactly the TypeScript baseline behavior. Completing instead would
    // INSERT a receipt row from a shape the apply never produced — a status
    // request must never manufacture appliedOps evidence. A later
    // `recordOutcome` still stands on its own, as in the baseline.
    const body: Json = { proposalId, from: ["applying"], to, patch: {} };
    if (patch?.appliedAt !== undefined)
      body["now"] = toEpoch(patch.appliedAt, "patch.appliedAt");
    else withNow(body, clock);
    await call("proposals.transition", body);
    return call("proposals.get", { proposalId });
  }
  const body: Json = { proposalId, operationId, outcome: recorded };
  if (patch?.appliedAt !== undefined) body["now"] = toEpoch(patch.appliedAt, "patch.appliedAt");
  else withNow(body, clock);
  await call("proposals.complete", body);
  return call("proposals.get", { proposalId });
}

/**
 * The engine stores each event as `{ taskId, seq, event, createdAt }` with
 * the caller's validated envelope nested under `event`; the port's log is the
 * envelope itself. Fresh parse per call, so a returned snapshot can never be
 * mutated back into the store.
 */
function projectEvent(value: unknown): TaskEventEnvelope {
  const source = record(value);
  if (source === null) return {} as TaskEventEnvelope;
  const envelope = "event" in source ? (record(source["event"]) ?? source) : source;
  return envelope as unknown as TaskEventEnvelope;
}

/** Port search markers (see `SEARCH_MATCH_START`/`_END`/`SNIPPET_ELLIPSIS`
 * in the conversation port); mirrored here to keep this module's host import
 * type-only. */
const SEARCH_MATCH_START = "[";
const SEARCH_MATCH_END = "]";
const SEARCH_SNIPPET_ELLIPSIS = "...";
const SNIPPET_CONTEXT_CHARS = 40;

/** The shared text projection: every `text` part of a multimodal body, never
 * just the first (see `searchTextOf` in the port docs). */
function searchTextOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter(
      (part) =>
        record(part) !== null && (record(part) as Json)["type"] === "text",
    )
    .map((part) => {
      const entry = record(part);
      const text = entry?.["text"];
      return typeof text === "string" ? text : "";
    })
    .join("\n");
}

/** A window around the first match, marked like the reference adapters. */
function snippetAround(text: string, at: number, length: number): string {
  const start = Math.max(0, at - SNIPPET_CONTEXT_CHARS);
  const end = Math.min(text.length, at + length + SNIPPET_CONTEXT_CHARS);
  return [
    start > 0 ? SEARCH_SNIPPET_ELLIPSIS : "",
    text.slice(start, at),
    SEARCH_MATCH_START,
    text.slice(at, at + length),
    SEARCH_MATCH_END,
    text.slice(at + length, end),
    end < text.length ? SEARCH_SNIPPET_ELLIPSIS : "",
  ].join("");
}

/**
 * Build every sub-port over one call surface (the root store or an open
 * transaction's view). All state — ordering, transitions, fencing, dedup —
 * is decided inside the engine; each method here is parse-and-project.
 */
function buildPorts(
  port: NativeStorePort,
  clock?: NativeAssistantStoreClock,
): Omit<AssistantStore, "transaction"> {
  const call = async <T,>(operation: string, input?: unknown): Promise<T> =>
    port.callStore<T>(operation, input ?? {});

  const conversations: ConversationStore = {
    createChat: async (input: CreateChatInput): Promise<ChatRecord> =>
      projectChat(await call("conversations.createChat", withNow({ input }, clock))),
    getChat: async (chatId: string): Promise<ChatRecord | null> => {
      const value = await call<unknown>("conversations.getChat", { chatId });
      return value === null ? null : projectChat(value);
    },
    listChats: async (opts?: ListChatsOptions): Promise<ChatRecord[]> => {
      const chats = await call<unknown[]>("conversations.listChats", { opts });
      return chats.map(projectChat);
    },
    updateChat: async (chatId: string, patch: UpdateChatPatch): Promise<ChatRecord> =>
      projectChat(await call("conversations.updateChat", withNow({ chatId, patch }, clock))),
    deleteChat: async (chatId: string): Promise<void> => {
      await call("conversations.deleteChat", { chatId });
    },
    appendMessage: async (input: AppendMessageInput, fence?: RunWriteFence) => {
      if (
        input.activate === false &&
        (input.parentMessageId === undefined || input.parentMessageId === null)
      ) {
        throw new NativeAgentError(
          "invalid_append",
          "appendMessage with activate: false must name the parentMessageId to chain off.",
        );
      }
      try {
        return projectMessage(
          await call(
            "conversations.appendMessage",
            withNow(fence === undefined ? { input } : { input, fence }, clock),
          ),
        );
      } catch (error) {
        // The engine owns tree legality; a refusal that names the port's own
        // rule (`invalid_fork_point`-style markers) keeps the port's code.
        throw reclassify(error, [["invalid_append", "invalid_append"]]);
      }
    },
    updateMessage: async (
      messageId: string,
      patch: UpdateMessagePatch,
      fence?: RunWriteFence,
    ): Promise<MessageRecord> =>
      projectMessage(
        await call(
          "conversations.updateMessage",
          withNow(fence === undefined ? { messageId, patch } : { messageId, patch, fence }, clock),
        ),
      ),
    listMessages: async (chatId: string, opts?: ListMessagesOptions) => {
      if (opts?.afterOrderKey !== undefined && opts?.beforeOrderKey !== undefined) {
        throw new NativeAgentError(
          "invalid_cursor",
          "listMessages accepts at most one of afterOrderKey / beforeOrderKey.",
        );
      }
      return projectMessages(await call("conversations.listMessages", { chatId, opts }));
    },
    getMessage: async (messageId: string): Promise<MessageRecord | null> => {
      const value = await call<unknown>("conversations.getMessage", { messageId });
      return value === null ? null : projectMessage(value);
    },
    listTree: async (chatId: string, options?: { includeArchived?: boolean }) =>
      projectMessages(await call("conversations.listTree", { chatId, options })),
    archiveBranch: async (messageId: string) => {
      try {
        return (
          (await call<{ archivedCount: number }>("conversations.archiveBranch", { messageId })) ?? {
            archivedCount: 0,
          }
        );
      } catch (error) {
        throw reclassify(error, [["active_branch_archive", "active_branch_archive"]]);
      }
    },
    lastMessageOfRun: async (chatId: string, runId: string) => {
      const value = await call<unknown>("conversations.lastMessageOfRun", { chatId, runId });
      return value === null ? null : projectMessage(value);
    },
    listSiblings: async (messageId: string) =>
      projectMessages(await call("conversations.listSiblings", { messageId })),
    activatePath: async (messageId: string) =>
      projectMessages(await call("conversations.activatePath", { messageId })),
    forkChat: async (chatId: string, fromMessageId: string): Promise<ForkChatResult> => {
      let value: Json;
      try {
        value = await call<Json>(
          "conversations.forkChat",
          withNow({ chatId, fromMessageId }, clock),
        );
      } catch (error) {
        throw reclassify(error, [["invalid_fork_point", "invalid_fork_point"]]);
      }
      return {
        chat: projectChat(value["chat"]),
        messages: projectMessages(value["messages"]),
      };
    },
    importConversation: async (input: ImportConversationInput): Promise<ChatRecord> =>
      projectChat(await call("conversations.importConversation", withNow({ input }, clock))),
    searchMessages: async (query: string, opts?: SearchMessagesOptions) => {
      const needle = query.trim().toLowerCase();
      if (needle.length === 0) return [];
      const rows = await call<unknown[]>("conversations.searchMessages", { query, opts });
      const hits: MessageSearchHit[] = [];
      for (const row of rows) {
        const source = record(row);
        const message = source === null ? null : record(source["message"]);
        if (message === null) continue;
        const chatId = message["chatId"];
        const messageId = message["id"];
        if (typeof chatId !== "string" || typeof messageId !== "string") continue;
        const text = searchTextOf(message["content"]);
        const at = text.toLowerCase().indexOf(needle);
        hits.push({
          chatId,
          messageId,
          snippet: at === -1 ? text : snippetAround(text, at, needle.length),
        });
      }
      return hits;
    },
  };

  const tasks: TaskStore = {
    createTask: async (input: CreateTaskInput): Promise<TaskRecord> => {
      if (input.availableAt !== undefined) {
        // Normalize early: an unparsable instant is refused with the port's
        // code before it can reach a comparison anywhere.
        toEpoch(input.availableAt, "availableAt");
      }
      try {
        return projectTask(await call("tasks.createTask", withNow({ input }, clock)));
      } catch (error) {
        throw reclassify(error, [["unknown dependency", "unknown_dependency"]]);
      }
    },
    getTask: async (taskId: string) => {
      const value = await call<unknown>("tasks.getTask", { taskId });
      return value === null ? null : projectTask(value);
    },
    interruptQueued: async (before: string) =>
      await call<number>("tasks.interruptQueued", { before: toEpoch(before, "before") }),
    interruptTask: async (taskId: string, reason: string, opts?: FencedWriteOptions) =>
      await call<boolean>(
        "tasks.interruptTask",
        withNow(opts === undefined ? { taskId, reason } : { taskId, reason, opts }, clock),
      ),
    listChildren: async (taskId: string) =>
      (await call<unknown[]>("tasks.listChildren", { taskId })).map(projectTask),
    listByScope: async (scopeId: string) =>
      (await call<unknown[]>("tasks.listByScope", { scopeId })).map(projectTask),
    deleteByScope: async (scopeId: string) =>
      await call<number>("tasks.deleteByScope", { scopeId }),
    transitionTask: async (
      taskId: string,
      from: TaskStatus[],
      to: TaskStatus,
      patch?: TaskPatch,
      opts?: FencedWriteOptions,
    ) =>
      projectTask(
        await call(
          "tasks.transitionTask",
          withNow(
            { taskId, from, to, ...(patch ? { patch } : {}), ...(opts ? { opts } : {}) },
            clock,
          ),
        ),
      ),
    createAttempt: async (input: CreateAttemptInput) =>
      projectAttempt(await call("tasks.createAttempt", withNow({ input }, clock))),
    endAttempt: async (input: EndAttemptInput) =>
      projectAttempt(await call("tasks.endAttempt", withNow({ input }, clock))),
    acquireLease: async (input) =>
      projectLease(await call("tasks.acquireLease", withNow({ input }, clock))),
    renewLease: async (leaseToken: string, ttlMs: number) =>
      projectLease(await call("tasks.renewLease", withNow({ leaseToken, ttlMs }, clock))),
    releaseLease: async (leaseToken: string) => {
      await call("tasks.releaseLease", { leaseToken });
    },
    expireStaleLeases: async (now: Date) =>
      (await call<unknown[]>("tasks.expireStaleLeases", { now: toEpoch(now, "now") })).map(
        projectLease,
      ),
    appendEvents: async (
      taskId: string,
      events: TaskEventEnvelope[],
      opts: AppendEventsOptions,
    ): Promise<void> => {
      // Port-level monotonicity/dedup: `seq` must strictly increase and an
      // `eventId` may appear once per task log — both refused with the port's
      // `seq_conflict`. The SQLite primary keys would reject too, but only
      // with an opaque constraint error, so the check lives here the same way
      // the reference adapters do it.
      const stored = (await call<unknown[]>("tasks.listEvents", { taskId })).map(projectEvent);
      let maxSeq = Number.NEGATIVE_INFINITY;
      const seen = new Set<string>();
      for (const entry of stored) {
        if (typeof entry.seq === "number" && entry.seq > maxSeq) maxSeq = entry.seq;
        if (typeof entry.eventId === "string") seen.add(entry.eventId);
      }
      for (const event of events) {
        const entry = record(event);
        const seq = entry?.["seq"];
        if (typeof seq !== "number" || !(seq > maxSeq)) {
          throw new NativeAgentError("seq_conflict", `event seq must be greater than ${maxSeq}.`);
        }
        const eventId = entry?.["eventId"];
        if (typeof eventId === "string" && seen.has(eventId)) {
          throw new NativeAgentError("seq_conflict", `event ${eventId} is already in the log.`);
        }
        maxSeq = seq;
        if (typeof eventId === "string") seen.add(eventId);
      }
      await call("tasks.appendEvents", withNow({ taskId, events, opts }, clock));
    },
    listEvents: async (taskId: string, opts?: ListEventsOptions) =>
      (await call<unknown[]>("tasks.listEvents", { taskId, opts })).map(projectEvent),
    updateProgress: async (
      taskId: string,
      progress: Record<string, unknown>,
      opts: UpdateProgressOptions,
    ) => projectTask(await call("tasks.updateProgress", withNow({ taskId, progress, opts }, clock))),
    nextSeq: async (taskId: string) => await call<number>("tasks.nextSeq", { taskId }),
    claimNext: async (input: ClaimNextInput): Promise<ClaimedTask | null> => {
      // "An empty kinds list means no kind is acceptable" (port contract) —
      // the engine's claim treats an empty filter as no filter, so this is
      // answered here, not delegated.
      if (input.kinds !== undefined && input.kinds.length === 0) return null;
      const claimed = await call<Json | null>("tasks.claimNext", {
        input: {
          ownerId: input.ownerId,
          now: toEpoch(input.now, "now"),
          scopesBusy: input.scopesBusy,
          ...(input.kinds === undefined ? {} : { kinds: input.kinds }),
        },
      });
      if (claimed === null) return null;
      return {
        task: projectTask(claimed["task"]),
        attempt: projectAttempt(claimed["attempt"]),
        lease: projectLease(claimed["lease"]),
      };
    },
    markDeadLettered: async (taskId: string, reason: string, opts?: FencedWriteOptions) =>
      projectTask(
        await call(
          "tasks.markDeadLettered",
          withNow(opts === undefined ? { taskId, reason } : { taskId, reason, opts }, clock),
        ),
      ),
  };

  const proposals: ProposalStore = {
    create: async (input: CreateProposalInput): Promise<ProposalRecord> => {
      // The (scopeKey, actionId) idempotency key is the port's promise and
      // the SQLite engine leaves its enforcement to adapters: a live proposal
      // (any status but rejected/invalidated) under the same key refuses.
      if (input.actionId !== undefined) {
        const holder = await call<unknown>("proposals.getByActionId", {
          scopeKey: input.scopeKey,
          actionId: input.actionId,
        });
        const held = record(holder);
        if (held !== null && !RELEASED_ACTION_STATUSES.includes(held["status"] as ProposalStatus)) {
          throw new NativeAgentError(
            "duplicate_action_id",
            `action_id ${input.actionId} already used in scope ${input.scopeKey}.`,
            { scopeKey: input.scopeKey, actionId: input.actionId, holderId: held["id"] },
          );
        }
      }
      return projectProposal(await call("proposals.create", withNow({ input }, clock)));
    },
    get: async (proposalId: string) => {
      const value = await call<unknown>("proposals.get", { proposalId });
      return value === null ? null : projectProposal(value);
    },
    getByActionId: async (scopeKey: string, actionId: string) => {
      const value = await call<unknown>("proposals.getByActionId", { scopeKey, actionId });
      return value === null ? null : projectProposal(value);
    },
    listByChat: async (chatId: string, opts?: ListProposalsOptions) =>
      (await call<unknown[]>("proposals.listByChat", { chatId, opts })).map(projectProposal),
    listByStatus: async (status: ProposalStatus, opts?: { limit?: number }) =>
      (await call<unknown[]>("proposals.listByStatus", { status, opts })).map(projectProposal),
    transition: async (
      proposalId: string,
      from: ProposalStatus[],
      to: ProposalStatus,
      patch?: ProposalPatch,
    ) => {
      // The engine splits the terminal apply edge out of the plain CAS
      // (`complete_proposal`, receipt-bound). Map the port's single
      // `transition` onto that path, reading the outcome from the receipt the
      // caller already recorded (or the operations-shaped default the apply
      // would have produced) and landing `appliedAt` as the engine stamp.
      if (to === "applied" || to === "failed") {
        return projectProposal(
          await settleApply(call, proposalId, from, to, patch, clock),
        );
      }
      const body: Json = { proposalId, from, to, ...(patch ? { patch } : {}) };
      const stamp =
        to === "applying"
          ? patch?.claimedAt
          : patch?.decision !== undefined
            ? patch?.decidedAt
            : undefined;
      if (stamp !== undefined) body["now"] = toEpoch(stamp, "patch timestamp");
      else withNow(body, clock);
      return projectProposal(await call("proposals.transition", body));
    },
    recordOutcome: async (operationId: string, outcome: ApplyOutcome) => {
      const stored = await call<unknown>("proposals.recordOutcome", withNow({ operationId, outcome }, clock));
      return projectOutcome(stored) as ApplyOutcome;
    },
    getOutcome: async (operationId: string) =>
      projectOutcome(await call("proposals.getOutcome", { operationId })),
    invalidatePendingForRevision: async (scopeKey: string, newRevision: string) =>
      await call<number>("proposals.invalidatePendingForRevision", { scopeKey, newRevision }),
    deleteByChat: async (chatId: string) =>
      await call<number>("proposals.deleteByChat", { chatId }),
  };

  const inputs: AgentInputStore = {
    createInput: async (input: CreateAgentInput) => {
      validateAgentInput(input);
      const created = await call<{ record: unknown; created: boolean }>(
        "inputs.createInput",
        withNow({ input }, clock),
      );
      return {
        record: projectInput(created.record) as AgentInputRecord,
        created: created.created,
      };
    },
    getInput: async (inputId: string) =>
      projectInput(await call("inputs.getInput", { inputId })),
    listInputs: async (query: AgentInputQuery) => {
      const rows = await call<unknown[]>("inputs.listInputs", { query });
      return rows.map((row) => projectInput(row) as AgentInputRecord);
    },
    transitionInput: async (
      inputId: string,
      from: readonly AgentInputStatus[],
      to: AgentInputStatus,
      patch?: AgentInputPatch,
    ) =>
      projectInput(
        await call(
          "inputs.transitionInput",
          withNow({ inputId, from: [...from], to, ...(patch ? { patch } : {}) }, clock),
        ),
      ) as AgentInputRecord,
    deleteSessionInputs: async (sessionId: string) =>
      await call<number>("inputs.deleteSessionInputs", { sessionId }),
  };

  const summaries: ContextSummaryStore = {
    // The store keeps the summary document verbatim (first write wins), so the
    // port record round-trips untouched.
    saveSummary: async (record: ContextSummaryRecord) => {
      await call("summaries.save", record);
    },
    listSummaries: async (sessionId: string) =>
      (await call<ContextSummaryRecord[]>("summaries.list", { sessionId })) ?? [],
    deleteSessionSummaries: async (sessionId: string) =>
      await call<number>("summaries.deleteSession", { sessionId }),
  };

  const providers: ProviderStore = {
    listProviders: async () => (await call<AiProviderConfig[]>("providers.list")) ?? [],
    getProvider: async (providerId: string) => {
      const value = await call<AiProviderConfig | null>("providers.get", { providerId });
      return value === null ? null : value;
    },
    upsertProvider: async (config: AiProviderConfig) =>
      await call<AiProviderConfig>("providers.upsert", config),
    deleteProvider: async (providerId: string) => {
      await call("providers.delete", { providerId });
    },
    listModels: async (providerId: string) =>
      (await call<AiProviderModel[]>("providers.listModels", { providerId })) ?? [],
    replaceModels: async (providerId: string, models: AiProviderModel[]) => {
      await call("providers.replaceModels", { providerId, models });
    },
    getCapabilities: async (providerId: string) =>
      (await call<AiProviderCapabilities | null>("providers.getCapabilities", {
        providerId,
      })) ?? null,
    saveCapabilities: async (providerId: string, capabilities) => {
      await call("providers.saveCapabilities", { providerId, capabilities });
    },
  };

  const settings: SettingsStore = {
    getSettings: async () => projectSettings(await call("settings.getSettings")),
    updateSettings: async (patch) =>
      projectSettings(await call("settings.updateSettings", { patch })),
  };

  const outbox: OutboxStore = {
    enqueue: async (input: OutboxAppendInput) =>
      projectOutbox(await call("outbox.enqueue", withNow({ input }, clock))),
    claimBatch: async (input: OutboxClaimInput) => {
      const claimed = await call<unknown[]>("outbox.claimBatch", {
        input: {
          limit: input.limit,
          now: toEpoch(input.now, "now"),
          ...(input.ownerId === undefined ? {} : { ownerId: input.ownerId }),
        },
      });
      return claimed.map(projectOutbox);
    },
    markPublished: async (id: string, at: Date) => {
      await call("outbox.markPublished", { id, at: toEpoch(at, "at") });
    },
    markFailed: async (id: string, error: string, retryAt: Date) => {
      await call("outbox.markFailed", { id, error, retryAt: toEpoch(retryAt, "retryAt") });
    },
    prune: async (before: Date) => await call<number>("outbox.prune", { before: toEpoch(before, "before") }),
  };

  const continuations: ProviderContinuationStore = {
    bindRun: async (runId: string, scope: ProviderContinuationScope, fence: RunWriteFence) => {
      try {
        await call("continuations.bindRun", withNow({ runId, scope, fence }, clock));
      } catch (error) {
        throw reclassify(error, [["provider_continuation_state_missing", "provider_continuation_state_missing"]]);
      }
    },
    getRunScope: async (runId: string) =>
      (await call<ProviderContinuationScope | null>("continuations.getRunScope", { runId })) ?? null,
    put: async (input: ProviderContinuationRecord, fence: RunWriteFence) => {
      try {
        return projectContinuation(await call("continuations.put", withNow({ input, fence }, clock)));
      } catch (error) {
        throw reclassify(error, [["provider_continuation_state_missing", "provider_continuation_state_missing"]]);
      }
    },
    getByRun: async (runId: string) => {
      try {
        const value = await call<unknown>("continuations.getByRun", { runId });
        return value === null ? null : projectContinuation(value);
      } catch (error) {
        throw reclassify(error, [["provider_continuation_state_missing", "provider_continuation_state_missing"]]);
      }
    },
    findByAnchors: async (chatId: string, anchorMessageIds: readonly string[]) =>
      (await call<unknown[]>("continuations.findByAnchors", {
        chatId,
        anchorMessageIds: [...anchorMessageIds],
      })).map(projectContinuation),
  };

  const toolExecutions: ToolExecutionStore = {
    admit: async (input: ToolExecutionAdmission, fence: RunWriteFence) =>
      await call<boolean>("toolExecutions.admit", withNow({ input, fence }, clock)),
    complete: async (executionId: string, result, fence: RunWriteFence) => {
      await call("toolExecutions.complete", withNow({ executionId, result, fence }, clock));
    },
    listByRun: async (runId: string) => {
      const rows = await call<unknown[]>("toolExecutions.listByRun", { runId });
      return rows.map((row) => {
        const source = record(row) ?? {};
        const out: Json = {};
        for (const key of [
          "executionId",
          "runId",
          "toolCallId",
          "toolName",
          "argsSha256",
          "contextSha256",
          "attemptId",
          "approvalRequestId",
          "approvalGeneration",
          "status",
          "result",
        ] as const) {
          copy(source, out, key);
        }
        return out as unknown as ToolExecutionRecord;
      });
    },
  };

  return { conversations, tasks, proposals, inputs, summaries, providers, settings, outbox, continuations, toolExecutions };
}

/**
 * The {@link AssistantStore} aggregate over the Rust SQLite engine.
 *
 * `transaction(fn)` opens a real binding transaction and hands `fn` a store
 * view whose EVERY sub-port call runs inside it; nested `tx.transaction()`
 * flattens into the open one (see `withNativeTransaction`). The port's
 * per-store caller serialization is the engine's transaction gate — a second
 * `transaction()` (or any root call) issued while one is open waits, bounded
 * by `NativeStore.setTransactionGateTimeout`.
 */
export class NativeAssistantStore implements AssistantStore {
  readonly conversations: ConversationStore;
  readonly tasks: TaskStore;
  readonly proposals: ProposalStore;
  readonly inputs: AgentInputStore;
  readonly summaries: ContextSummaryStore;
  readonly providers: ProviderStore;
  readonly settings: SettingsStore;
  readonly outbox: OutboxStore;
  readonly continuations: ProviderContinuationStore;
  readonly toolExecutions: ToolExecutionStore;

  private readonly port: NativeStorePort;
  private readonly clock: NativeAssistantStoreClock | undefined;

  constructor(options: NativeAssistantStoreOptions) {
    const port =
      options.port ??
      options.store ??
      (options.raw !== undefined ? createNativeStorePort(options.raw) : undefined);
    if (port === undefined) {
      throw new NativeAgentError(
        "invalid_input",
        "NativeAssistantStore needs one of { store } | { raw } | { port }.",
      );
    }
    this.port = port;
    this.clock = options.clock;
    const ports = buildPorts(port, options.clock);
    this.conversations = ports.conversations;
    this.tasks = ports.tasks;
    this.proposals = ports.proposals;
    this.inputs = ports.inputs!;
    this.summaries = ports.summaries!;
    this.providers = ports.providers;
    this.settings = ports.settings;
    this.outbox = ports.outbox;
    this.continuations = ports.continuations!;
    this.toolExecutions = ports.toolExecutions!;
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    return this.port.transaction((view) =>
      fn(new NativeAssistantStore({ port: view, clock: this.clock })),
    );
  }
}