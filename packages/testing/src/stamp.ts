import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";

/**
 * Distributive `Omit` — plain `Omit<AiRunEvent, ...>` would collapse the union
 * into one object type with merged `data`, losing the discriminant. Duplicated
 * from `@agentkit/core`'s `events.ts` rather than imported: this package must
 * stay `@agentkit/core`-runtime-free (core is a peer dependency only), so the
 * handful of lines below are re-implemented against `@agentkit/contracts`
 * alone.
 */
type WithoutBase<E> = E extends AiRunEvent
  ? Omit<E, "contractVersion" | "eventId" | "seq" | "attemptId">
  : never;

/**
 * A run event before the transport base fields are stamped on — the shape test
 * doubles in this package build before {@link createTestEventStamper} turns it
 * into an `AiRunEvent`.
 */
export type TestEventDraft = WithoutBase<AiRunEvent>;

export interface TestEventStamperOptions {
  /** First `seq` value. Default 0. */
  firstSeq?: number;
  /** Stamped on every event; groups the events of one attempt of a run. */
  attemptId?: string;
}

export type TestEventStamper = (event: TestEventDraft) => AiRunEvent;

/** ISO-8601 "now", for events that need a timestamp before they're stamped. */
export function nowIso(): string {
  return new Date().toISOString();
}

/**
 * Test-double mirror of `@agentkit/core`'s `createEventStamper`: assigns
 * `contractVersion`, a fresh `eventId`, and the next `seq` to each event handed
 * to it, exactly the same stamping contract the real run-loop applies.
 *
 * Implemented against `@agentkit/contracts` only — no runtime dependency on
 * `@agentkit/core` — so the mock provider clients in this package can emit
 * contract-valid `AiRunEvent`s while standing in for a real provider, without
 * pulling in the runtime they exist to stand in for.
 */
export function createTestEventStamper(
  opts: TestEventStamperOptions = {},
): TestEventStamper {
  let seq = opts.firstSeq ?? 0;
  const { attemptId } = opts;
  return (event: TestEventDraft): AiRunEvent =>
    ({
      ...event,
      contractVersion: CONTRACT_VERSION,
      eventId: crypto.randomUUID(),
      seq: seq++,
      ...(attemptId === undefined ? {} : { attemptId }),
    }) as AiRunEvent;
}
