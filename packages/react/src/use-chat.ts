/**
 * `useChat` — one conversation's active path, plus the four writes that change
 * it.
 *
 * THE SHAPE OF THE PROBLEM. A turn is not a request/response: `submitMessage`
 * returns in milliseconds with three ids and no answer, the answer arrives over
 * SSE for the next several seconds, and the durable truth about what was
 * written lands in the store as the events are projected. A hook that only
 * rendered server reads would show nothing until the run finished; one that
 * only rendered the stream would lose everything on a remount. So this renders
 * BOTH, in that order: an optimistic pair the moment the user hits send, deltas
 * applied to the placeholder as they arrive, and then — once the run is
 * terminal — the server's own `listMessages` replacing the lot.
 *
 * THE RECONCILE IS NOT OPTIONAL, and it is why the local delta application can
 * afford to be simple. The stream shows the user their answer as it is typed;
 * the server decides what the conversation actually contains (the internal
 * tool records, the `placeholder: false` flip, a correction pass's rewrite).
 * Anything this hook got wrong while streaming survives for at most one round
 * trip.
 *
 * WHAT `submit` RESOLVES ON: the run being ACCEPTED, not finished. A form that
 * awaited the whole answer before clearing its input would hold the input
 * hostage for the length of the turn. Follow `status`/`phase` for the rest.
 */
import {
  createRunPhaseTracker,
  newIdempotencyKey,
  type AgentKitClient,
  type AgentKitClientError,
  type RunPhase,
} from "@agentkit/client";
import type {
  AiRunEvent,
  MessageDto,
  RegenerateMessageRequest,
  SubmitMessageRequest,
} from "@agentkit/contracts";
import { useCallback, useEffect, useRef } from "react";
import { useAgentKitClient, useAgentKitContext } from "./context.js";
import { chatTopic, nextOrigin } from "./emitter.js";
import {
  isAbort,
  toError,
  useAliveRef,
  useMirroredState,
  useTopicSubscription,
} from "./internal.js";
import {
  appendDelta,
  loadActivePath,
  nextOptimisticIds,
  optimisticPair,
  replaceMessage,
  truncateAfter,
  type PagingOptions,
} from "./messages.js";

/**
 * The coarse "is something in flight" signal.
 *
 * - `loading` — a read or a write is out and nothing is streaming yet.
 * - `streaming` — a run is live and this hook is following it. That INCLUDES a
 *   run parked on an approval; `phase` is the field that tells them apart.
 * - `error` — {@link UseChatResult.error} says what went wrong. The messages
 *   already loaded are still there.
 */
export type ChatStatus = "idle" | "loading" | "streaming" | "error";

export interface ChatState {
  /** The chat's active path, oldest first, verbatim as the server projects it. */
  messages: MessageDto[];
  status: ChatStatus;
  /** The live run's derived phase, or `null` when no run has been followed yet. */
  phase: RunPhase | null;
  activeRunId: string | null;
  error: AgentKitClientError | Error | null;
  /**
   * `true` when {@link UseChatOptions.maxPages} stopped the read before the end
   * of the conversation. {@link ChatState.messages} then ends BEFORE the newest
   * turn — including whatever the user just sent — so a UI that renders it as a
   * whole conversation is lying about what is there. See
   * {@link PagingOptions.maxPages}.
   */
  truncated: boolean;
}

export interface UseChatOptions extends PagingOptions {
  /** Talk to this client instead of the provider's. The provider is still required. */
  client?: AgentKitClient;
}

export interface SubmitOptions {
  /** Override the provider's default model for this turn. */
  model?: string;
  /**
   * Branch under this message instead of appending to the active leaf — the
   * edit-and-regenerate write. {@link UseChatResult.editAndResubmit} is the
   * named spelling of it.
   */
  parentMessageId?: string;
  metadata?: Record<string, unknown>;
  /**
   * Replay a specific `Idempotency-Key`. Rarely needed: a failed submit parks
   * its own key and the next identical submit reuses it. See
   * {@link UseChatResult.submit}.
   */
  idempotencyKey?: string;
}

export interface UseChatResult extends ChatState {
  /**
   * Send a turn. Resolves when the server ACCEPTS it; the answer streams after.
   *
   * The optimistic user message and the empty assistant placeholder are in
   * `messages` before this function's first `await`, so a component that
   * re-renders on the click already shows them.
   *
   * IDEMPOTENCY. A key is minted per submit and returned to the server with it.
   * If the call FAILS, that key is parked: calling `submit` again with the same
   * content and the same `parentMessageId` replays the same key rather than
   * asking the question twice, which is the recovery a "send failed, retry?"
   * button needs. Different content mints a fresh key — a replayed key against
   * a different question would answer the OLD one.
   */
  submit(
    content: SubmitMessageRequest["content"],
    opts?: SubmitOptions,
  ): Promise<void>;
  /**
   * Answer the same question again, as a sibling of `messageId`.
   *
   * The old answer keeps its id and its `branchIndex` and stops being active;
   * `useBranches(messageId)` is how a user switches back. Resolves once the new
   * branch is active and re-read — the message list already shows the new
   * placeholder when it does.
   */
  regenerate(
    messageId: string,
    opts?: RegenerateMessageRequest & { idempotencyKey?: string },
  ): Promise<void>;
  /**
   * Rewrite a question and answer it again on a new branch.
   *
   * `parentMessageId` is `messageId` — the new turn hangs UNDER the named
   * message, per `SubmitMessageRequest.parentMessageId`. The path is cut there
   * optimistically and re-read from the server on accept.
   */
  editAndResubmit(
    messageId: string,
    content: SubmitMessageRequest["content"],
    opts?: Omit<SubmitOptions, "parentMessageId">,
  ): Promise<void>;
  /**
   * Ask the server to stop the live run.
   *
   * It does NOT tear down the local stream: the run answers with its own
   * `run.cancelled` event, and letting that arrive is what leaves the hook with
   * a correct final phase and a reconciled message list. Aborting here instead
   * would leave a cancelled run rendered as if it were still typing.
   */
  cancel(): Promise<void>;
  /** Re-read the active path from the server. */
  reload(): Promise<void>;
}

const EMPTY: ChatState = {
  messages: [],
  status: "idle",
  phase: null,
  activeRunId: null,
  error: null,
  truncated: false,
};

export function useChat(
  chatId: string | null,
  options: UseChatOptions = {},
): UseChatResult {
  const { emitter } = useAgentKitContext();
  const client = useAgentKitClient(options.client);
  const alive = useAliveRef();
  const { value, read, update } = useMirroredState<ChatState>(EMPTY);

  const pageSize = options.pageSize;
  const maxPages = options.maxPages;

  // Lazily minted once per hook instance; `useRef(nextOrigin())` would burn a
  // counter value on every render for a value only the first render keeps.
  const originRef = useRef<string | null>(null);
  if (originRef.current === null) originRef.current = nextOrigin("chat");
  const origin = originRef.current;

  /** The live run's stream, so a new turn or an unmount can end it. */
  const streamRef = useRef<AbortController | null>(null);
  /**
   * The chat this hook is CURRENTLY rendering, readable from a follow that
   * started under a different one. Written at commit rather than during render,
   * for the reason `useMirroredState` gives: a render concurrent React throws
   * away must not be able to move it.
   */
  const chatIdRef = useRef<string | null>(chatId);
  /** A failed submit's key, held for the retry of the SAME question. */
  const parkedRef = useRef<{ key: string; signature: string } | null>(null);

  const refresh = useCallback(
    async (signal?: AbortSignal): Promise<void> => {
      if (chatId === null) return;
      update((prev) =>
        prev.status === "idle" || prev.status === "error"
          ? { ...prev, status: "loading" }
          : prev,
      );
      try {
        const path = await loadActivePath(
          client,
          chatId,
          {
            ...(pageSize === undefined ? {} : { pageSize }),
            ...(maxPages === undefined ? {} : { maxPages }),
          },
          signal,
        );
        if (signal?.aborted === true) return;
        update((prev) => ({
          ...prev,
          messages: path.items,
          truncated: path.truncated,
          // Only the read this call OWNS settles back to idle: a reconcile in
          // the middle of a live run must leave `streaming` alone, and the
          // error it clears is the one the retry just disproved.
          ...(prev.status === "loading"
            ? { status: "idle" as const, error: null }
            : {}),
        }));
      } catch (cause) {
        if (isAbort(cause, signal)) return;
        update((prev) => ({
          ...prev,
          status: "error",
          error: toError(cause),
        }));
      }
    },
    [client, chatId, pageSize, maxPages, update],
  );

  /**
   * Follow a run to its end, then hand the message list back to the server.
   *
   * Never rejects: every failure it can see becomes `status: "error"`, because
   * the caller is a click handler and an unhandled rejection out of one is a
   * console error nobody owns.
   *
   * EVERY WRITE IS GUARDED by the chat this follow belongs to. The run outlives
   * the render that started it, and `chatId` is captured — so without the guard
   * a follow started in chat A goes on applying A's deltas after the user has
   * switched to chat B, and finishes by writing A's `listMessages` into B's
   * state. The switch aborts the stream as well; the guard is what covers the
   * work already in flight when it lands.
   */
  const followRun = useCallback(
    async (runId: string, placeholderId: string): Promise<void> => {
      if (chatId === null) return;
      const chat = chatId;
      streamRef.current?.abort();
      const controller = new AbortController();
      streamRef.current = controller;

      /** Still this hook's chat, still mounted, still the live stream. */
      const owns = (): boolean =>
        alive.current &&
        chatIdRef.current === chat &&
        !controller.signal.aborted;

      const events: AiRunEvent[] = [];
      // The phase, folded per event instead of rescanned per event: see
      // `createRunPhaseTracker`.
      const tracker = createRunPhaseTracker();
      let streamed = false;
      let lastEventId: string | undefined;

      try {
        for await (const event of client.streamRun(runId, {
          signal: controller.signal,
        })) {
          events.push(event);
          lastEventId = event.eventId;
          const phase = tracker.observe(event);
          // A new pass throws away what the last one said — the host clears the
          // stored answer at this exact seam — so the placeholder starts over
          // and so does the "did this pass stream?" flag the
          // `run.message.completed` rule below reads.
          const boundary = tracker.startedNewPass();
          if (boundary) streamed = false;
          if (event.type === "run.message.delta") streamed = true;
          if (!owns()) return;
          applyEvent(update, placeholderId, event, phase, streamed, boundary);
        }
        // The stream closed because the TASK is terminal, and anything written
        // in the same breath as that transition — the harness's
        // `run.verification`, a late warning — may be on the log without having
        // been delivered. One resumed pass collects it; without it a corrected
        // run reports the phase it had before the correction ran.
        const drained = await client.drainRun(runId, lastEventId, {
          signal: controller.signal,
        });
        events.push(...drained);
        for (const event of drained) tracker.observe(event);
        if (!owns()) return;

        await refresh(controller.signal);
        if (!owns()) return;

        const phase = tracker.phase();
        const failure = phase === "failed" ? runFailure(events) : null;
        update((prev) => ({
          ...prev,
          phase,
          activeRunId: null,
          status: phase === "failed" ? "error" : "idle",
          error: failure ?? (phase === "failed" ? prev.error : null),
        }));
      } catch (cause) {
        if (isAbort(cause, controller.signal) || !owns()) return;
        update((prev) => ({
          ...prev,
          status: "error",
          error: toError(cause),
          phase: tracker.phase(),
          activeRunId: null,
        }));
      } finally {
        if (streamRef.current === controller) streamRef.current = null;
        if (alive.current) emitter.emit(chatTopic(chat), { origin });
      }
    },
    [client, chatId, refresh, update, alive, emitter, origin],
  );

  const submit = useCallback<UseChatResult["submit"]>(
    async (content, opts = {}) => {
      if (chatId === null) {
        update((prev) => ({ ...prev, status: "error", error: noChatId() }));
        return;
      }

      const ids = nextOptimisticIds();
      const before = read().messages;
      const base =
        opts.parentMessageId === undefined
          ? before
          : truncateAfter(before, opts.parentMessageId);
      const pair = optimisticPair({
        chatId,
        ids,
        content,
        ...(opts.metadata === undefined ? {} : { metadata: opts.metadata }),
      });
      update((prev) => ({
        ...prev,
        messages: [...base, pair.user, pair.assistant],
        status: "loading",
        phase: "queued",
        activeRunId: null,
        error: null,
      }));

      // The key is minted HERE rather than left to the client, which mints one
      // too — but only returns it with a successful answer, and the call this
      // needs a key for is the one that did not answer.
      const signature = submitSignature(content, opts.parentMessageId);
      const parked = parkedRef.current;
      const idempotencyKey =
        opts.idempotencyKey ??
        (parked !== null && parked.signature === signature
          ? parked.key
          : newIdempotencyKey());

      try {
        const submitted = await client.submitMessage(
          { chatId },
          {
            content,
            ...(opts.model === undefined ? {} : { model: opts.model }),
            ...(opts.parentMessageId === undefined
              ? {}
              : { parentMessageId: opts.parentMessageId }),
            ...(opts.metadata === undefined ? {} : { metadata: opts.metadata }),
          },
          { idempotencyKey },
        );
        parkedRef.current = null;
        if (!alive.current) return;

        const { result } = submitted;
        update((prev) => ({
          ...prev,
          messages: prev.messages.map((message) =>
            message.id === ids.user
              ? adopt(message, result.userMessageId, result.runId, {})
              : message.id === ids.assistant
                ? adopt(message, result.assistantMessageId, result.runId, {
                    placeholder: true,
                  })
                : message,
          ),
          activeRunId: result.runId,
          status: "streaming",
          phase: "queued",
        }));
        emitter.emit(chatTopic(chatId), { origin });

        // A branch submit moved the active path; the truncation above was this
        // hook's guess at it and the server's answer is the fact.
        if (opts.parentMessageId !== undefined) await refresh();
        void followRun(result.runId, result.assistantMessageId);
      } catch (cause) {
        // The key survives the failure so the retry lands on the same turn.
        parkedRef.current = { key: idempotencyKey, signature };
        if (!alive.current) return;
        update((prev) => ({
          ...prev,
          // Rollback: the two optimistic records described a write that never
          // happened. Removed BY ID rather than by restoring the list as it was
          // before the await — that snapshot is several seconds stale by now,
          // and putting it back would also discard whatever landed while this
          // submit was out: a delta on a still-running turn, a second submit's
          // own optimistic pair, a reconcile.
          messages: prev.messages.filter(
            (message) =>
              message.id !== ids.user && message.id !== ids.assistant,
          ),
          status: "error",
          phase: null,
          activeRunId: null,
          error: toError(cause),
        }));
      }
    },
    [chatId, client, read, update, alive, emitter, origin, refresh, followRun],
  );

  const regenerate = useCallback<UseChatResult["regenerate"]>(
    async (messageId, opts = {}) => {
      if (chatId === null) {
        update((prev) => ({ ...prev, status: "error", error: noChatId() }));
        return;
      }
      update((prev) => ({
        ...prev,
        status: "loading",
        phase: "queued",
        error: null,
      }));
      try {
        const { idempotencyKey, ...body } = opts;
        const { result } = await client.regenerateMessage(
          { chatId, messageId },
          body,
          idempotencyKey === undefined ? undefined : { idempotencyKey },
        );
        if (!alive.current) return;
        update((prev) => ({
          ...prev,
          activeRunId: result.runId,
          status: "streaming",
        }));
        emitter.emit(chatTopic(chatId), { origin });
        // The new sibling is the active one now, and it is not in the list this
        // hook is holding — there is nothing optimistic to show, so read.
        await refresh();
        void followRun(result.runId, result.assistantMessageId);
      } catch (cause) {
        if (!alive.current) return;
        update((prev) => ({
          ...prev,
          status: "error",
          phase: null,
          activeRunId: null,
          error: toError(cause),
        }));
      }
    },
    [chatId, client, update, alive, emitter, origin, refresh, followRun],
  );

  const editAndResubmit = useCallback<UseChatResult["editAndResubmit"]>(
    (messageId, content, opts = {}) =>
      submit(content, { ...opts, parentMessageId: messageId }),
    [submit],
  );

  const cancel = useCallback<UseChatResult["cancel"]>(async () => {
    const runId = read().activeRunId;
    if (runId === null) return;
    try {
      await client.cancelRun({ runId });
    } catch (cause) {
      if (!alive.current) return;
      update((prev) => ({ ...prev, status: "error", error: toError(cause) }));
    }
  }, [client, read, update, alive]);

  const reload = useCallback<UseChatResult["reload"]>(
    () => refresh(),
    [refresh],
  );

  // The initial read, and every re-read a changed chat id calls for. The abort
  // makes it StrictMode-safe: the first effect's fetch is discarded rather than
  // racing the second's for the last word.
  useEffect(() => {
    if (chatId === null) {
      update(() => EMPTY);
      return;
    }
    const controller = new AbortController();
    void refresh(controller.signal);
    return () => controller.abort();
  }, [chatId, refresh, update]);

  // Somebody else changed this chat — a branch switch, another component's
  // submit. Our own announcements are skipped: this hook has already re-read.
  useTopicSubscription(
    emitter,
    chatId === null ? null : chatTopic(chatId),
    (event) => {
      if (event.origin === origin) return;
      void refresh();
    },
  );

  // A stream outlives a render but must not outlive the component OR THE CHAT.
  // With `[]` deps this only fired on unmount, and switching chats mid-run left
  // the old run streaming into a hook that had moved on — deltas applied to a
  // placeholder that is no longer on screen, and a terminal reconcile that
  // replaced the new chat's messages with the old chat's.
  //
  // The reset is part of the same fact: the run this hook was following is no
  // longer this hook's business, so no `activeRunId` and no `streaming` may
  // survive into the next chat. `update` is already a no-op after unmount
  // (`useAliveRef`'s cleanup runs first), so this touches state on a chat
  // switch only.
  useEffect(() => {
    chatIdRef.current = chatId;
    return () => {
      streamRef.current?.abort();
      streamRef.current = null;
      update((prev) =>
        prev.activeRunId === null && prev.status !== "streaming"
          ? prev
          : { ...prev, activeRunId: null, phase: null, status: "idle" },
      );
    };
  }, [chatId, update]);

  return {
    ...value,
    submit,
    regenerate,
    editAndResubmit,
    cancel,
    reload,
  };
}

/**
 * One run event, applied to the placeholder.
 *
 * MIRRORS THE HOST's own projector (`packages/host/src/turn/projection.ts`)
 * rather than inventing a second rule: deltas accumulate, and a
 * `run.message.completed` overwrites the text only for a NON-streaming provider
 * (one that sent no deltas at all). Overwriting after a stream would drop the
 * earlier iterations of a tool-calling turn, whose visible answer is the
 * concatenation of every pass — and the host, which is what the reconcile will
 * hand back, keeps the concatenation.
 */
function applyEvent(
  update: (next: (prev: ChatState) => ChatState) => void,
  placeholderId: string,
  event: AiRunEvent,
  phase: RunPhase,
  streamed: boolean,
  boundary: boolean,
): void {
  update((prev) => {
    let messages = prev.messages;
    if (boundary) {
      // Mirrors `TurnRunner.resetPass` + its `updateMessage({ content: "" })`:
      // the previous pass's half sentence is not part of the answer the server
      // will hand back, and appending pass 2 to it reads as one rambling reply
      // until the reconcile lands.
      messages = replaceMessage(messages, placeholderId, (message) => ({
        ...message,
        content: "",
      }));
    }
    if (event.type === "run.message.delta") {
      messages = replaceMessage(messages, placeholderId, (message) =>
        appendDelta(message, event.data.delta),
      );
    } else if (
      event.type === "run.message.completed" &&
      event.data.toolCallCount === 0 &&
      !streamed &&
      event.data.content.length > 0
    ) {
      const content = event.data.content;
      messages = replaceMessage(messages, placeholderId, (message) => ({
        ...message,
        content,
      }));
    }
    return {
      ...prev,
      messages,
      phase,
      // A boundary is proof the run is live again, so it clears a `status` a
      // terminal event of the previous pass (or a failed reconcile) had set.
      status: !boundary && prev.status === "error" ? "error" : "streaming",
    };
  });
}

/**
 * The `run.failed` event's own message, so `error` says what the RUN said.
 *
 * Scanned from the END, because a run can hold several: only the last one is
 * the outcome the phase reports, and an earlier pass's failure is a step on the
 * way to it.
 */
function runFailure(events: readonly AiRunEvent[]): Error | null {
  for (let at = events.length - 1; at >= 0; at -= 1) {
    const event = events[at];
    if (event === undefined || event.type !== "run.failed") continue;
    const error = new Error(event.data.errorMessage);
    error.name = event.data.errorCode ?? "RunFailed";
    return error;
  }
  return null;
}

/** An optimistic record taking on the id the server gave it. */
function adopt(
  message: MessageDto,
  id: string,
  runId: string,
  metadata: Record<string, unknown>,
): MessageDto {
  const { optimistic: _dropped, ...rest } = message.metadata;
  return { ...message, id, runId, metadata: { ...rest, ...metadata } };
}

/** What makes two submits "the same question" for idempotency-key reuse. */
function submitSignature(
  content: SubmitMessageRequest["content"],
  parentMessageId: string | undefined,
): string {
  return JSON.stringify([content, parentMessageId ?? null]);
}

function noChatId(): Error {
  return new Error(
    "useChat(null) has no chat to write to. Create one with " +
      "client.createChat() and pass its id before calling submit/regenerate.",
  );
}
