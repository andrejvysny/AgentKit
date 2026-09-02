/**
 * `useRun` — one run's event log, live.
 *
 * The difference from {@link useChat} is what it renders: `useChat` renders a
 * CONVERSATION and treats the event stream as the means, `useRun` renders the
 * STREAM — the tool calls, the warnings, the usage, the verification passes. It
 * is what a run inspector, a debug drawer or a "what is it doing right now"
 * panel is built on, and it is deliberately read-only.
 *
 * RESUME COMES FOR FREE and is the reason this is worth a hook at all:
 * `client.streamRun` reconnects with `Last-Event-ID` on a dropped connection
 * and yields every event exactly once across the seam, so a laptop that slept
 * through the middle of a run comes back to a complete log rather than a hole.
 *
 * The subscription is an EFFECT, which makes it SSR-safe (it never runs on the
 * server) and StrictMode-safe (a doubled effect aborts its first stream, and
 * every append is de-duplicated by `eventId` in case one delivered before the
 * abort landed).
 *
 * THE EVENTS ARE NOT THE LAST WORD ON THE PHASE. The stream closes when the
 * TASK is terminal, and a task can go terminal without a terminal event on the
 * log — the host's `failQuietly` writes one only best-effort. So when the log
 * ends without saying how, this asks the run (see `settlePhase`); otherwise a
 * quietly failed run renders as "still typing" indefinitely.
 */
import {
  createRunPhaseTracker,
  type AgentKitClient,
  type AgentKitClientError,
  type RunPhase,
  type RunPhaseTracker,
} from "@agentkit/client";
import type { AiRunEvent } from "@agentkit/contracts";
import { useCallback, useEffect, useRef } from "react";
import { useAgentKitClient } from "./context.js";
import {
  finishReasonOf,
  isAbort,
  quietFailure,
  settlePhase,
  toError,
  useAliveRef,
  useMirroredState,
} from "./internal.js";

export interface RunState {
  /** Every event seen so far, `seq` order, no duplicates. */
  events: AiRunEvent[];
  phase: RunPhase | null;
  error: AgentKitClientError | Error | null;
  /**
   * Why the last pass stopped — the `finishReason` of its `run.completed` (or
   * `run.message.completed`), `null` before one has arrived.
   *
   * Worth rendering because `"incomplete"` is a real answer the contract
   * refuses to launder: the provider's stream was cut before it said why, and
   * the run is `completed` with a TRUNCATED answer. A UI that shows only the
   * phase presents that as a finished reply.
   */
  finishReason: string | null;
}

export interface UseRunOptions {
  client?: AgentKitClient;
}

export interface UseRunResult extends RunState {
  /**
   * One resumed pass past the last event seen, for what a live stream cannot
   * deliver: the host writes `run.verification` AFTER the terminal event, and
   * the stream closed at the terminal event. Safe to call more than once.
   */
  drain(): Promise<void>;
}

const EMPTY: RunState = {
  events: [],
  phase: null,
  error: null,
  finishReason: null,
};

export function useRun(
  runId: string | null,
  options: UseRunOptions = {},
): UseRunResult {
  const client = useAgentKitClient(options.client);
  const alive = useAliveRef();
  const { value, read, update } = useMirroredState<RunState>(EMPTY);

  /** eventIds already appended — the de-dup a doubled effect needs. */
  const seenRef = useRef<Set<string>>(new Set());
  /** The phase as a running tally, so no arrival costs a rescan of the log. */
  const trackerRef = useRef<RunPhaseTracker>(createRunPhaseTracker());
  /** The last pass's `finishReason`; a pass boundary is what clears it. */
  const finishReasonRef = useRef<string | null>(null);

  const ingest = useCallback(
    (incoming: readonly AiRunEvent[]): void => {
      const fresh = incoming.filter(
        (event) => !seenRef.current.has(event.eventId),
      );
      if (fresh.length === 0) return;
      for (const event of fresh) seenRef.current.add(event.eventId);
      for (const event of fresh) {
        trackerRef.current.observe(event);
        // The previous pass's reason is not this pass's: the host abandoned
        // what it said and asked again.
        if (trackerRef.current.startedNewPass()) finishReasonRef.current = null;
        const reason = finishReasonOf(event);
        if (reason !== undefined) finishReasonRef.current = reason;
      }
      const phase = trackerRef.current.phase();
      const finishReason = finishReasonRef.current;
      update((prev) => {
        // Inserted in `seq` order rather than re-sorted: every event but the
        // handful either side of a resume seam belongs at the end, and sorting
        // the whole log per token makes a long run quadratic.
        const events = [...prev.events];
        for (const event of fresh) insertBySeq(events, event);
        return { ...prev, events, phase, finishReason };
      });
    },
    [update],
  );

  useEffect(() => {
    seenRef.current = new Set();
    trackerRef.current = createRunPhaseTracker();
    finishReasonRef.current = null;
    update(() => EMPTY);
    if (runId === null) return;

    const controller = new AbortController();
    void (async () => {
      const events: AiRunEvent[] = [];
      try {
        for await (const event of client.streamRun(runId, {
          signal: controller.signal,
        })) {
          if (controller.signal.aborted || !alive.current) return;
          events.push(event);
          ingest([event]);
        }
      } catch (cause) {
        if (isAbort(cause, controller.signal) || !alive.current) return;
        update((prev) => ({ ...prev, error: toError(cause) }));
        return;
      }
      // The stream closed because the TASK is terminal — which does not oblige
      // the log to hold a terminal event. Ask the run what happened when it
      // does not, or a quietly failed run renders as "still typing" for good.
      const settled = await settlePhase(
        client,
        runId,
        trackerRef.current,
        events,
        controller.signal,
      );
      if (controller.signal.aborted || !alive.current) return;
      // A status-decided failure has no `run.failed` to quote, so the hook owes
      // an `error` of its own: `phase: "failed"` next to `error: null` reads as
      // "it failed and nobody knows anything", which is worse than saying so.
      const quiet =
        settled.fromStatus &&
        (settled.phase === "failed" || settled.phase === "cancelled");
      update((prev) => ({
        ...prev,
        phase: settled.phase,
        error: quiet && prev.error === null ? quietFailure() : prev.error,
      }));
    })();

    return () => controller.abort();
  }, [runId, client, ingest, update, alive]);

  const drain = useCallback<UseRunResult["drain"]>(async () => {
    if (runId === null) return;
    const lastEventId = read().events.at(-1)?.eventId;
    try {
      ingest(await client.drainRun(runId, lastEventId));
    } catch (cause) {
      if (!alive.current) return;
      update((prev) => ({ ...prev, error: toError(cause) }));
    }
  }, [runId, client, read, ingest, update, alive]);

  return { ...value, drain };
}

/**
 * Place one event in a `seq`-ordered list, scanning back from the newest end.
 *
 * That end is where the answer almost always is — a live stream arrives in
 * order — so the common case is one comparison. Ties keep arrival order, which
 * is what the stable sort this replaces did.
 *
 * An event with no usable `seq` is APPENDED. Every comparison against a
 * non-number is false, so the scan below would walk all the way to index 0 and
 * splice one mangled frame in FRONT of a run's whole log — the most misleading
 * possible place for it. Arrival order is the only ordering left to honour.
 */
function insertBySeq(events: AiRunEvent[], event: AiRunEvent): void {
  if (!Number.isFinite(event.seq)) {
    events.push(event);
    return;
  }
  let at = events.length;
  for (; at > 0; at -= 1) {
    const previous = events[at - 1];
    if (previous === undefined || previous.seq <= event.seq) break;
    // One already in the list that has no usable `seq` stops the scan as well:
    // every comparison against it is false, so walking PAST it would move
    // well-formed events in front of a frame that arrived before them.
    if (!Number.isFinite(previous.seq)) break;
  }
  events.splice(at, 0, event);
}
