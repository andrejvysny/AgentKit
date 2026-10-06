import { canonicalMessage } from "./canonical-projection.js";
import { recordRunUsage } from "./record-run-usage.js";
import type { AiRunEvent, AiToolCall } from "@agentkit/contracts";
import { AgentKitHostError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, Logger } from "../ports/system.js";
import type { TaskRecord } from "../ports/task-store.js";
import type { UsageAuthorizer } from "../ports/usage-authorizer.js";

/**
 * Durable event projection shared by local turns and custom host executors.
 * Executors own finalization and must persist the final message, attempt, and
 * terminal task in one fenced transaction, with terminal task status last.
 */
export interface RunProjectorDeps {
  store: AssistantStore;
  clock: Clock;
  /**
   * Told about every `run.usage` event projected, with the provider's own
   * numbers. Absent, nothing is recorded — exactly as on a `TurnRunner` with no
   * usage port wired.
   */
  usage?: UsageAuthorizer;
  logger?: Logger;
}

/** What {@link RunProjector.createState} is told about the turn it projects. */
export interface RunProjectionStateInput {
  /** Keep canonical Responses turns separately from the displayed answer. */
  preserveCanonicalTurns?: boolean;
  chatId: string;
  /** The empty assistant record this run streams its visible answer into. */
  assistantMessageId: string;
  /**
   * The id {@link RunProjectorDeps.usage} bills a `run.usage` event against.
   * Omitted — a host that has no provider of its own to name — it is reported
   * as the empty string rather than the event being dropped: an unattributed
   * usage record still counts against a budget, a missing one silently does not.
   */
  providerId?: string;
}

/**
 * Mutable state accumulated across the events of ONE run.
 *
 * Created by {@link RunProjector.createState} and threaded through every
 * {@link RunProjector.project} call. It is deliberately a plain object the
 * caller holds: `TurnRunner` reads `content`/`toolCallIds` to make its retry
 * decisions between passes, and a host executor reads the same fields to decide
 * what to write into the placeholder at the end.
 */
export interface RunProjectionState {
  readonly preserveCanonicalTurns?: boolean;
  readonly chatId: string;
  readonly assistantMessageId: string;
  readonly providerId?: string;
  content: string;
  /** Bounded replay cursor; sequential events are applied at most once. */
  projectedSeq: number;
  streamed: boolean;
  /**
   * Distinct tool call ids seen SINCE THE LAST PASS BOUNDARY — the "did it use
   * tools?" signal. `TurnRunner.resetPass` clears it with the rest of the
   * answer-so-far, because every reader of it (the `empty_response` warning,
   * the emulated-call detector, `VerificationInput.toolCallCount`) is asking
   * about the pass that produced the answer, and a retry runs with no registry.
   */
  toolCallIds: Set<string>;
  /** Internal assistant record awaiting its tool calls (see `project`). */
  pendingAssistantMessageId?: string;
  pendingToolCalls: AiToolCall[];
  /**
   * Tool calls announced BEFORE the assistant turn that declared them was
   * completed — a client that emits `run.tool.requested` DURING the message
   * rather than after it.
   *
   * `runChat` never does this (it completes the turn, then announces), but the
   * projector is a public seam a host drives from its own executor, and a
   * bridge mapping some other provider's frames may well see the calls first.
   * Without this buffer that ordering persists the assistant record with no
   * `toolCalls` at all, and every tool result after it replays as an orphan
   * `tool_call_id` — which providers reject outright.
   */
  announcedToolCalls: AiToolCall[];
  /**
   * Placeholder-write coalescing state — see {@link createRunProjector}.
   *
   * `content` is updated on every delta; the DURABLE write behind it is
   * throttled, and these two fields are the throttle. `unflushedDeltas > 0`
   * means the stored record is behind `content`. Public because
   * `TurnRunner.resetPass` has to DISCARD a pending write it is about to
   * supersede with an empty answer — writing it afterwards would put the
   * abandoned pass's text back.
   */
  unflushedDeltas: number;
  /** {@link Clock.now} in epoch ms at the last durable placeholder write. */
  lastFlushAtMs: number;
  /**
   * The last message THIS RUN wrote — the link every further append chains off.
   *
   * Seeded with the placeholder, so the run's records descend from the answer
   * they belong to, and carried across passes because a retry continues the
   * same conversation branch rather than starting a second one.
   *
   * It exists because "the chat's active leaf" is not a stable answer for the
   * duration of a turn: a user may switch branches between two of these
   * writes, and an append that took the leaf would put the second half of this
   * run's records on a conversation that never ran them — while leaving this
   * run's own branch with tool calls nobody answered. Naming the link removes
   * the race rather than narrowing it. See `AppendMessageInput.activate`.
   */
  lastMessageId: string;
}

/**
 * The run whose log is being written: the same three fields every durable write
 * in a task attempt carries. It is a structural subset of
 * `TaskExecutionContext`, so an executor passes `ctx` straight through.
 */
export interface RunProjectionContext {
  task: TaskRecord;
  attemptId: string;
  leaseToken: string;
}

export interface RunProjector {
  createState(input: RunProjectionStateInput): RunProjectionState;
  /**
   * Append one ALREADY-STAMPED event to the task's durable log, then reflect it
   * into conversation state.
   *
   * STAMPED, not a draft: while a chat pass is streaming, core's
   * `createEventStamper` owns the numbering — it was handed a `firstSeq` and
   * counts upward in memory — and a projector that re-numbered from
   * `TaskStore.nextSeq` would interleave two counters into one log. So the
   * caller that produced the events is the caller that numbers them, and this
   * appends what it is given verbatim. A host holding raw drafts instead wants
   * {@link createRunEventFeed}.
   *
   * The log commits before projection. Repeated event identities are accepted
   * only when their durable envelopes match. A projection failure keeps the
   * event available for fenced, idempotent replay.
   */
  project(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void>;
  /**
   * The fenced conversation half of {@link RunProjector.project}, WITHOUT the append —
   * for a caller that has already put the event on the log itself.
   *
   * {@link createRunEventFeed} is the only caller in this package; it exists so
   * the feed can delegate the numbering-and-append to `createTaskEventWriter`
   * without the event landing on the log twice. A host that appends its own
   * events (its own writer, its own batching) uses this for the same reason.
   */
  /** Rebuild from the durable log; event-backed messages keep stable identities. */
  replay(ctx: RunProjectionContext, state: RunProjectionState): Promise<void>;
  reflect(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void>;
}

/** Bound projection writes while the durable log retains every delta. */
const DELTA_FLUSH_MAX_DELTAS = 32;
/** …or this many milliseconds, whichever comes first (a slow trickle still lands). */
const DELTA_FLUSH_INTERVAL_MS = 50;

/**
 * Keep run-owned records on their branch using explicit parent IDs.
 * Coalesce visible deltas, retaining every event in the durable log. Replay
 * reconstructs lagging content and reuses deterministic internal message IDs.
 * Each projection transaction and message mutation validates current ownership.
 */
export function createRunProjector(deps: RunProjectorDeps): RunProjector {
  const { store } = deps;

  /** Flush only content that has not yet reached its durable projection. */
  async function flushContent(
    store: AssistantStore,
    ctx: RunProjectionContext,
    state: RunProjectionState,
  ): Promise<void> {
    if (state.unflushedDeltas === 0) return;
    await store.conversations.updateMessage(
      state.assistantMessageId,
      {
        content: state.content,
      },
      { taskId: ctx.task.taskId, leaseToken: ctx.leaseToken },
    );
    // AFTER the write, not before: "unflushed" has to stay true while the write
    // is outstanding, or a throw here would leave the state claiming a durable
    // answer that was never written.
    state.unflushedDeltas = 0;
    state.lastFlushAtMs = deps.clock.now().getTime();
  }

  async function reflectEvent(
    store: AssistantStore,
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void> {
    const { task } = ctx;
    const chatId = state.chatId;
    const fence = { taskId: task.taskId, leaseToken: ctx.leaseToken };
    const messageId = `run-event:${task.taskId}:${event.eventId}`;
    const append = async (
      input: Parameters<typeof store.conversations.appendMessage>[0],
    ) => {
      const existing = await store.conversations.getMessage(messageId);
      if (existing) return existing;
      return store.conversations.appendMessage(
        { ...input, id: messageId },
        fence,
      );
    };

    // Decision events observe durable text even when the last delta was coalesced.
    if (event.type !== "run.message.delta")
      await flushContent(store, ctx, state);

    switch (event.type) {
      case "run.warning": {
        if (event.data.code === "retry_pass") {
          state.content = "";
          state.streamed = false;
          state.toolCallIds.clear();
          state.unflushedDeltas = 0;
          delete state.pendingAssistantMessageId;
          state.pendingToolCalls = [];
          state.announcedToolCalls = [];
          await store.conversations.updateMessage(
            state.assistantMessageId,
            { content: "" },
            fence,
          );
        }
        break;
      }
      case "run.message.delta": {
        state.content += event.data.delta;
        state.streamed = true;
        state.unflushedDeltas += 1;
        const elapsed = deps.clock.now().getTime() - state.lastFlushAtMs;
        if (
          state.unflushedDeltas >= DELTA_FLUSH_MAX_DELTAS ||
          elapsed >= DELTA_FLUSH_INTERVAL_MS
        ) {
          await flushContent(store, ctx, state);
        }
        break;
      }
      case "run.message.completed": {
        if (state.preserveCanonicalTurns) {
          await canonicalMessage(store, ctx, state, event, append);
          break;
        }
        // A new assistant turn supersedes any turn still waiting for its calls:
        // late `run.tool.requested` events belong to THIS turn, not the last one.
        delete state.pendingAssistantMessageId;
        state.pendingToolCalls = [];
        // Calls announced during THIS message, before it completed. Taken
        // (and cleared) whether or not they end up used, so they cannot leak
        // into the next assistant turn.
        const announced = state.announcedToolCalls;
        state.announcedToolCalls = [];
        if (event.data.toolCallCount > 0) {
          // The event's own list wins when it has one — it is the authoritative
          // report of what the turn asked for. The buffer is the fallback for a
          // client that announced the calls instead of carrying them here.
          const toolCalls =
            event.data.toolCalls ?? (announced.length > 0 ? announced : []);
          const record = await append({
            chatId,
            runId: task.taskId,
            role: "assistant",
            content: event.data.content,
            toolCalls,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true },
          });
          state.lastMessageId = record.id;
          for (const call of toolCalls) state.toolCallIds.add(call.id);
          if (toolCalls.length < event.data.toolCallCount) {
            // Keep the assistant open for later call announcements so replay
            // cannot treat its subsequent tool results as orphans.
            state.pendingAssistantMessageId = record.id;
            state.pendingToolCalls = [...toolCalls];
          }
        } else if (!state.streamed && event.data.content.length > 0) {
          // Non-streaming provider: the visible answer exists only here.
          state.content = event.data.content;
          await store.conversations.updateMessage(
            state.assistantMessageId,
            {
              content: state.content,
            },
            fence,
          );
        }
        break;
      }
      case "run.tool.requested": {
        state.toolCallIds.add(event.data.toolCallId);
        const call = {
          id: event.data.toolCallId,
          name: event.data.toolName,
          argumentsJson: event.data.argumentsJson,
        };
        if (state.pendingAssistantMessageId !== undefined) {
          state.pendingToolCalls.push(call);
          await store.conversations.updateMessage(
            state.pendingAssistantMessageId,
            { toolCalls: [...state.pendingToolCalls] },
            fence,
          );
        } else {
          // No assistant record is open: this call was announced DURING the
          // message that declared it. Hold it until `run.message.completed`
          // creates the record — see `RunProjectionState.announcedToolCalls`.
          state.announcedToolCalls.push(call);
        }
        break;
      }
      case "run.tool.succeeded": {
        // The SLIM envelope is what gets persisted as the tool message, because
        // this record is replayed into the model's context on every later turn.
        // The full payload stays on the event, where the UI can read it once.
        const slim = event.data.modelResultJson ?? event.data.resultJson;
        state.lastMessageId = (
          await append({
            chatId,
            runId: task.taskId,
            role: "tool",
            content: slim,
            toolCallId: event.data.toolCallId,
            modelResultJson: slim,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true, toolName: event.data.toolName },
          })
        ).id;
        break;
      }
      case "run.tool.failed": {
        const slim =
          event.data.modelResultJson ??
          JSON.stringify({
            ok: false,
            status: "error",
            summary: event.data.errorMessage,
            warnings: [],
            truncated: false,
            data: {
              errorCode: event.data.errorCode,
              errorMessage: event.data.errorMessage,
            },
          });
        state.lastMessageId = (
          await append({
            chatId,
            runId: task.taskId,
            role: "tool",
            content: slim,
            toolCallId: event.data.toolCallId,
            modelResultJson: slim,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true, toolName: event.data.toolName },
          })
        ).id;
        break;
      }
      default:
        break;
    }
  }

  async function reflect(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
    meter = true,
  ): Promise<void> {
    let snapshot: RunProjectionState | undefined;
    let applied = false;
    try {
      await store.transaction(async (tx) => {
        await tx.tasks.appendEvents(ctx.task.taskId, [], {
          leaseToken: ctx.leaseToken,
        });
        if (event.seq <= state.projectedSeq) return;
        snapshot = structuredClone(state);
        await reflectEvent(tx, ctx, state, event);
        state.projectedSeq = event.seq;
        applied = true;
      });
    } catch (error) {
      if (snapshot) {
        const current = state as unknown as Record<string, unknown>;
        for (const key of Object.keys(current))
          if (!(key in snapshot)) delete current[key];
        Object.assign(state, snapshot);
      }
      throw error;
    }
    if (applied && meter && event.type === "run.usage")
      await recordRunUsage(deps, ctx, state, event);
  }

  return {
    createState(input: RunProjectionStateInput): RunProjectionState {
      return {
        preserveCanonicalTurns: input.preserveCanonicalTurns,
        chatId: input.chatId,
        assistantMessageId: input.assistantMessageId,
        ...(input.providerId === undefined
          ? {}
          : { providerId: input.providerId }),
        content: "",
        projectedSeq: -1,
        streamed: false,
        toolCallIds: new Set<string>(),
        pendingToolCalls: [],
        announcedToolCalls: [],
        unflushedDeltas: 0,
        lastFlushAtMs: deps.clock.now().getTime(),
        // Seeded with the placeholder: the run's records descend from the
        // answer they belong to.
        lastMessageId: input.assistantMessageId,
      };
    },

    async project(ctx, state, event): Promise<void> {
      const existing = (
        await store.tasks.listEvents(ctx.task.taskId, {
          afterSeq: event.seq - 1,
          limit: 1,
        })
      )[0];
      if (existing?.seq === event.seq) {
        if (
          existing.eventId !== event.eventId ||
          JSON.stringify(existing) !== JSON.stringify(event)
        ) {
          throw new AgentKitHostError(
            "projection_event_conflict",
            "Persisted event differs from the requested projection.",
          );
        }
      } else {
        await store.tasks.appendEvents(ctx.task.taskId, [event], {
          leaseToken: ctx.leaseToken,
        });
      }
      await reflect(ctx, state, event);
    },

    async replay(ctx, state): Promise<void> {
      let afterSeq = -1;
      for (;;) {
        const events = await store.tasks.listEvents(ctx.task.taskId, {
          afterSeq,
          limit: 256,
        });
        if (events.length === 0) {
          await store.transaction(async (tx) => {
            await tx.tasks.appendEvents(ctx.task.taskId, [], {
              leaseToken: ctx.leaseToken,
            });
            await flushContent(tx, ctx, state);
          });
          return;
        }
        for (const event of events) {
          await reflect(ctx, state, event as AiRunEvent, false);
          afterSeq = event.seq;
        }
      }
    },

    reflect,
  };
}

export {
  createRunEventFeed,
  type RunEventFeed,
  type RunEventFeedDeps,
} from "./run-event-feed.js";
