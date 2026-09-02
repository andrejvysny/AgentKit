import type { AiRunEvent, AiToolCall } from "@agentkit/contracts";
import type { AiRunEventDraft } from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator, Logger } from "../ports/system.js";
import type { TaskRecord, TaskStore } from "../ports/task-store.js";
import type { UsageAuthorizer } from "../ports/usage-authorizer.js";
import {
  createTaskEventWriter,
  type TaskEventDraft,
} from "../tasks/task-event-writer.js";

/**
 * The event → conversation projection a chat turn performs, as a seam a host
 * can drive from its OWN executor.
 *
 * `TurnRunner` is one caller of this module: it drives `runChat` and hands
 * every event it yields to {@link RunProjector.project}. A host whose turn does
 * not come from `runChat` at all — a cloud-delegated chat that maps remote
 * frames into {@link AiRunEvent}s, a replay of a recorded run, a bridge to a
 * provider SDK this package has no client for — registers its own
 * {@link TaskExecutor} for its own kind and drives the same projector. The
 * resulting conversation is indistinguishable from a `chat.turn`'s, because it
 * is produced by the same code rather than by a second implementation of the
 * same rules.
 *
 * WHAT THE HOST STILL OWNS, and this module deliberately does not:
 *
 * - producing the events (from wherever), and their `seq` numbering — see
 *   {@link RunProjector.project} on why they arrive already stamped, and
 *   {@link createRunEventFeed} for the drafts-in convenience;
 * - the placeholder's finalization (`content` + `placeholder: false`) at the
 *   end of the turn, and the task's terminal transition and `endAttempt`. Those
 *   are decisions about the RUN, not about an event, and an executor that
 *   wanted a different terminal (a delegated turn that lands `waiting_approval`)
 *   would have to fight a projector that had already settled it.
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
  readonly chatId: string;
  readonly assistantMessageId: string;
  readonly providerId?: string;
  /** Visible answer so far. */
  content: string;
  /** Whether any delta arrived (a completed-only provider sends none). */
  streamed: boolean;
  /** Distinct tool call ids seen this run — the "did it use tools?" signal. */
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
   * The log is written FIRST and unconditionally: it is the record of what
   * happened, and a projection failure must not be able to erase it.
   */
  project(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void>;
  /**
   * The conversation half of {@link RunProjector.project}, WITHOUT the append —
   * for a caller that has already put the event on the log itself.
   *
   * {@link createRunEventFeed} is the only caller in this package; it exists so
   * the feed can delegate the numbering-and-append to `createTaskEventWriter`
   * without the event landing on the log twice. A host that appends its own
   * events (its own writer, its own batching) uses this for the same reason.
   */
  reflect(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void>;
}

/**
 * A durable placeholder write happens at most once per this many deltas…
 *
 * 32 tokens is a fraction of a sentence, so a reader watching the message grow
 * sees no difference; the DB does 30× fewer writes for a long answer. The event
 * log is untouched by this — every delta is still appended, in order, before
 * anything here runs — so the coalescing is invisible to a consumer following
 * the stream, and the placeholder it lags is reconstructible from the log at
 * any point.
 */
const DELTA_FLUSH_MAX_DELTAS = 32;
/** …or this many milliseconds, whichever comes first (a slow trickle still lands). */
const DELTA_FLUSH_INTERVAL_MS = 50;

/**
 * Build the projection seam `TurnRunner` uses, over the same ports it takes.
 *
 * Every message it writes is a CHAIN append — `parentMessageId` is the id this
 * run wrote last, `activate: false` — so the records land on the run's own
 * branch whatever the user has been doing to the active path meanwhile. With no
 * branch switch that is byte-identically what an unparented append produced:
 * the run's last write IS the active leaf, so the chain and the path are the
 * same messages. See {@link RunProjectionState.lastMessageId}.
 *
 * DELTA WRITES ARE COALESCED. `run.message.delta` used to mean one
 * `updateMessage` each — a 2000-token answer is 2000 UPDATEs on the same row,
 * for a value only the last of which anybody reads. The stored placeholder is a
 * PROJECTION of the log, not the record of what happened, so it is safe to let
 * it lag: it is rewritten at most every {@link DELTA_FLUSH_MAX_DELTAS} deltas
 * or {@link DELTA_FLUSH_INTERVAL_MS} milliseconds, and flushed before ANY
 * non-delta event (which includes `run.message.completed` and every terminal),
 * so nothing that reads state — the retry decisions, the terminal placeholder
 * write, a `listMessages` after the turn — can ever see a stale answer. What a
 * crash can cost is under 50 ms of half-written text in a record the next
 * attempt overwrites anyway, and the log still has every delta.
 */
export function createRunProjector(deps: RunProjectorDeps): RunProjector {
  const { store } = deps;

  /**
   * Write the answer-so-far, if it is not already written. Cheap and idempotent
   * — `unflushedDeltas === 0` means the record already matches `content`.
   */
  async function flushContent(state: RunProjectionState): Promise<void> {
    if (state.unflushedDeltas === 0) return;
    await store.conversations.updateMessage(state.assistantMessageId, {
      content: state.content,
    });
    // AFTER the write, not before: "unflushed" has to stay true while the write
    // is outstanding, or a throw here would leave the state claiming a durable
    // answer that was never written.
    state.unflushedDeltas = 0;
    state.lastFlushAtMs = deps.clock.now().getTime();
  }

  async function reflect(
    ctx: RunProjectionContext,
    state: RunProjectionState,
    event: AiRunEvent,
  ): Promise<void> {
    const { task } = ctx;
    const chatId = state.chatId;

    // Anything that is not a delta is a decision point — the turn completed, a
    // tool ran, the run ended — and every one of them either reads the answer
    // or hands it to somebody who will. Flushing here rather than in each case
    // is what makes "the placeholder is never stale outside a delta burst" true
    // by construction instead of by remembering to add a flush to each branch.
    if (event.type !== "run.message.delta") await flushContent(state);

    switch (event.type) {
      case "run.message.delta": {
        state.content += event.data.delta;
        state.streamed = true;
        state.unflushedDeltas += 1;
        const elapsed = deps.clock.now().getTime() - state.lastFlushAtMs;
        if (
          state.unflushedDeltas >= DELTA_FLUSH_MAX_DELTAS ||
          elapsed >= DELTA_FLUSH_INTERVAL_MS
        ) {
          await flushContent(state);
        }
        break;
      }
      case "run.message.completed": {
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
          const record = await store.conversations.appendMessage({
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
            // A streaming provider reports the COUNT here and the calls
            // themselves in the `run.tool.requested` events that follow. Hold
            // the record open and fill them in as they arrive: an assistant
            // turn persisted without its tool_calls leaves every tool result
            // after it an orphan on the next replay. Also covers the partial
            // case — some calls announced early, the rest still to come.
            state.pendingAssistantMessageId = record.id;
            state.pendingToolCalls = [...toolCalls];
          }
        } else if (!state.streamed && event.data.content.length > 0) {
          // Non-streaming provider: the visible answer exists only here.
          state.content = event.data.content;
          await store.conversations.updateMessage(state.assistantMessageId, {
            content: state.content,
          });
        }
        break;
      }
      case "run.usage": {
        // Reported to the spend port AFTER the event is durable, and with the
        // provider's own numbers rather than the estimate the pass was
        // authorized on — the gap between the two is the whole reason
        // `UsageAuthorizer` has a second method. Every usage event is reported,
        // including the non-final ones a streaming provider emits mid-call: a
        // recorder that only saw `finalForCall` would lose the accounting for a
        // call that died before it settled, which is exactly the call a budget
        // most needs to know about. `finalForCall`/`source`/`step` ride along
        // so the recorder can tell those two kinds apart — reporting the
        // interim numbers without them is just double counting.
        //
        // GUARDED. `record()` is host bookkeeping — a counter, an HTTP call to
        // a metering service — and it runs mid-stream, inside the run's own
        // event loop. A throw out of here does not fail an accounting job; it
        // fails the TURN, after the provider has already been paid, taking the
        // answer down with it. The event is already durable on the log by the
        // time this runs, so a host that missed a `record()` can replay it; a
        // user whose answer vanished cannot.
        try {
          await deps.usage?.record({
            runId: task.taskId,
            callId: event.data.callId,
            attempt: event.data.attempt,
            providerId: state.providerId ?? "",
            model: event.data.model,
            finalForCall: event.data.finalForCall,
            source: event.data.source,
            step: event.data.step,
            ...(event.data.promptTokens === undefined
              ? {}
              : { promptTokens: event.data.promptTokens }),
            ...(event.data.completionTokens === undefined
              ? {}
              : { completionTokens: event.data.completionTokens }),
            ...(event.data.totalTokens === undefined
              ? {}
              : { totalTokens: event.data.totalTokens }),
            at: event.timestamp,
          });
        } catch (err) {
          deps.logger?.warn("usage record failed", {
            taskId: task.taskId,
            callId: event.data.callId,
            error: err instanceof Error ? err.message : String(err),
          });
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
          await store.conversations.appendMessage({
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
          await store.conversations.appendMessage({
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

  return {
    createState(input: RunProjectionStateInput): RunProjectionState {
      return {
        chatId: input.chatId,
        assistantMessageId: input.assistantMessageId,
        ...(input.providerId === undefined
          ? {}
          : { providerId: input.providerId }),
        content: "",
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
      await store.tasks.appendEvents(ctx.task.taskId, [event], {
        leaseToken: ctx.leaseToken,
      });
      await reflect(ctx, state, event);
    },

    reflect,
  };
}

/** Stamp a draft onto the run's log and project it, in one call. */
export interface RunEventFeed {
  emit(draft: AiRunEventDraft): Promise<AiRunEvent>;
}

export interface RunEventFeedDeps {
  projector: RunProjector;
  ctx: RunProjectionContext;
  state: RunProjectionState;
  tasks: TaskStore;
  clock: Clock;
  ids: IdGenerator;
}

/**
 * The drafts-in convenience over {@link RunProjector}: stamp, append, project.
 *
 * For a host executor that produces events one at a time and has no stamper of
 * its own — a cloud-delegated turn mapping remote frames as they arrive. The
 * numbering is `createTaskEventWriter`'s, not a second copy of it: `seq` comes
 * from `TaskStore.nextSeq` per emit, which is correct precisely because the
 * lease serializes writers, so there is one emitter at a time.
 *
 * TWO THINGS IT IS NOT FOR.
 *
 * A pass that already has a stamper — anything driving `runChat`, or a host
 * mapping a whole recorded run at once — must keep using that stamper and
 * {@link RunProjector.project}. Two counters over one log is the failure
 * `createTaskEventWriter` documents: both read the same "next" value between
 * appends, and the log either rejects the collision or silently reorders what a
 * client already received.
 *
 * And it stamps `timestamp` from {@link RunEventFeedDeps.clock}, so a draft's
 * own timestamp is replaced. A host that must preserve an upstream event's time
 * stamps with core's `createEventStamper` (which does not) and calls
 * {@link RunProjector.project} itself.
 */
export function createRunEventFeed(deps: RunEventFeedDeps): RunEventFeed {
  const writer = createTaskEventWriter({
    tasks: deps.tasks,
    taskId: deps.ctx.task.taskId,
    attemptId: deps.ctx.attemptId,
    leaseToken: deps.ctx.leaseToken,
    clock: deps.clock,
    ids: deps.ids,
  });
  return {
    async emit(draft: AiRunEventDraft): Promise<AiRunEvent> {
      const event = (await writer.emit(
        draft as unknown as TaskEventDraft,
      )) as unknown as AiRunEvent;
      // `reflect`, not `project`: the writer has already appended it, and a
      // second append would collide on `seq`.
      await deps.projector.reflect(deps.ctx, deps.state, event);
      return event;
    },
  };
}
