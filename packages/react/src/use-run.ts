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
import { isAbort, toError, useAliveRef, useMirroredState } from "./internal.js";

export interface RunState {
  /** Every event seen so far, `seq` order, no duplicates. */
  events: AiRunEvent[];
  phase: RunPhase | null;
  error: AgentKitClientError | Error | null;
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

const EMPTY: RunState = { events: [], phase: null, error: null };

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

  const ingest = useCallback(
    (incoming: readonly AiRunEvent[]): void => {
      const fresh = incoming.filter(
        (event) => !seenRef.current.has(event.eventId),
      );
      if (fresh.length === 0) return;
      for (const event of fresh) seenRef.current.add(event.eventId);
      for (const event of fresh) trackerRef.current.observe(event);
      const phase = trackerRef.current.phase();
      update((prev) => {
        // Inserted in `seq` order rather than re-sorted: every event but the
        // handful either side of a resume seam belongs at the end, and sorting
        // the whole log per token makes a long run quadratic.
        const events = [...prev.events];
        for (const event of fresh) insertBySeq(events, event);
        return { ...prev, events, phase };
      });
    },
    [update],
  );

  useEffect(() => {
    seenRef.current = new Set();
    trackerRef.current = createRunPhaseTracker();
    update(() => EMPTY);
    if (runId === null) return;

    const controller = new AbortController();
    void (async () => {
      try {
        for await (const event of client.streamRun(runId, {
          signal: controller.signal,
        })) {
          if (controller.signal.aborted || !alive.current) return;
          ingest([event]);
        }
      } catch (cause) {
        if (isAbort(cause, controller.signal) || !alive.current) return;
        update((prev) => ({ ...prev, error: toError(cause) }));
      }
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
 */
function insertBySeq(events: AiRunEvent[], event: AiRunEvent): void {
  let at = events.length;
  for (; at > 0; at -= 1) {
    const previous = events[at - 1];
    if (previous === undefined || previous.seq <= event.seq) break;
  }
  events.splice(at, 0, event);
}
