import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import { newEventId } from "./ids.js";

/**
 * Distributive `Omit` — plain `Omit<AiRunEvent, ...>` would collapse the union
 * into one object type with merged `data`, losing the discriminant.
 */
type WithoutBase<E> = E extends AiRunEvent
  ? Omit<E, "contractVersion" | "eventId" | "seq" | "attemptId">
  : never;

/**
 * A run event before the transport base fields are stamped on. Emitters build
 * these; {@link createEventStamper} turns them into `AiRunEvent`s.
 */
export type AiRunEventDraft = WithoutBase<AiRunEvent>;

export interface EventStamperOptions {
  /**
   * First `seq` value. Continue a resumed stream by passing the next number
   * after the last one already delivered, so the consumer's ordering key stays
   * unbroken across the seam. Default 0.
   */
  firstSeq?: number;
  /** Stamped on every event; groups the events of one attempt of a run. */
  attemptId?: string;
}

export type EventStamper = (event: AiRunEventDraft) => AiRunEvent;

/**
 * Build a stamper that assigns `contractVersion`, a fresh `eventId`, and the
 * next `seq` to each event it is handed.
 *
 * Sequence numbers come from a counter, not a clock: timestamps repeat within a
 * millisecond and clocks move, so they cannot tell a consumer that it missed an
 * event. A monotonic `seq` can.
 *
 * Re-stamping an already-stamped event overwrites its version, id and seq — the
 * run-loop does exactly that to the events it re-yields from a provider client,
 * so one run emits one unbroken sequence no matter how many clients contributed
 * to it. An `attemptId` already on the event survives when the stamper has none.
 */
export function createEventStamper(
  options: EventStamperOptions = {},
): EventStamper {
  let seq = options.firstSeq ?? 0;
  const { attemptId } = options;
  return (event: AiRunEventDraft): AiRunEvent =>
    ({
      ...event,
      contractVersion: CONTRACT_VERSION,
      eventId: newEventId(),
      seq: seq++,
      ...(attemptId === undefined ? {} : { attemptId }),
    }) as AiRunEvent;
}
