import type { AiRunEvent } from "@agentkit/contracts";
// `with { type: "json" }` (TS 5.3+, preserved verbatim through compilation)
// is required, not decorative: Node 20+ throws ERR_IMPORT_ATTRIBUTE_MISSING
// on a bare JSON import with no attribute, so a published `@agentkit/testing`
// dist would fail to even load under plain Node without it — bun does not
// need this, which is exactly why it went unnoticed under `bun test`.
import chatOnlyTrace from "./traces/chat-only.json" with { type: "json" };
import toolRunTrace from "./traces/tool-run.json" with { type: "json" };
import cancelledRunTrace from "./traces/cancelled-run.json" with {
  type: "json",
};
import failedRunTrace from "./traces/failed-run.json" with { type: "json" };
import usageRunTrace from "./traces/usage-run.json" with { type: "json" };

/**
 * The recorded golden scenarios. See `scripts/record-goldens.ts` at the repo
 * root for how each one was produced and how to regenerate them.
 */
export const GOLDEN_TRACE_NAMES = [
  "chat-only",
  "tool-run",
  "cancelled-run",
  "failed-run",
  "usage-run",
] as const;
export type GoldenTraceName = (typeof GOLDEN_TRACE_NAMES)[number];

// Statically imported (not a filesystem read keyed on process.cwd()), so
// resolution is done by the module graph at THIS file's location and works
// identically under `bun test` no matter which package directory the test
// runner was invoked from. `resolveJsonModule` widens field types (e.g. a
// literal "run.started" reads back as `string`), hence the double assertion.
const TRACES: Record<GoldenTraceName, AiRunEvent[]> = {
  "chat-only": chatOnlyTrace as unknown as AiRunEvent[],
  "tool-run": toolRunTrace as unknown as AiRunEvent[],
  "cancelled-run": cancelledRunTrace as unknown as AiRunEvent[],
  "failed-run": failedRunTrace as unknown as AiRunEvent[],
  "usage-run": usageRunTrace as unknown as AiRunEvent[],
};

/**
 * Load a recorded golden run-event trace by name. Traces are committed JSON,
 * produced once by `scripts/record-goldens.ts` (repo root) driving
 * `@agentkit/core`'s `runChat` against this package's mocks under fixed
 * inputs — the timestamps and event ids inside them are frozen history, not
 * regenerated on read.
 */
export function loadGoldenTrace(name: GoldenTraceName): AiRunEvent[] {
  return TRACES[name];
}

/**
 * Volatile per-event fields that differ on every recording — and on every live
 * replay of the same scenario — even when the run's *behavior* is identical:
 * wall-clock timestamps and randomly generated event ids. Stripping them is
 * what makes a live run comparable to a trace frozen in git history.
 */
export function normalizeTrace(
  events: readonly AiRunEvent[],
): Array<Omit<AiRunEvent, "timestamp" | "eventId">> {
  return events.map((event) => {
    const { timestamp: _timestamp, eventId: _eventId, ...rest } = event;
    // Round-trip through JSON so a live in-memory event — which may carry a
    // key explicitly set to `undefined` (e.g. an optional `data` field a
    // provider mock always assigns, present-but-undefined until echoed) —
    // compares equal to the same event loaded back from a golden trace file,
    // where JSON.stringify already dropped that key when it was recorded.
    return JSON.parse(JSON.stringify(rest)) as Omit<
      AiRunEvent,
      "timestamp" | "eventId"
    >;
  });
}

/** Recursive, key-order-independent structural equality over JSON-shaped values. */
function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a === null || b === null || a === undefined || b === undefined)
    return a === b;
  if (typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b)) return false;
    return a.length === b.length && a.every((item, i) => deepEqual(item, b[i]));
  }
  const aRecord = a as Record<string, unknown>;
  const bRecord = b as Record<string, unknown>;
  const aKeys = Object.keys(aRecord).sort();
  const bKeys = Object.keys(bRecord).sort();
  if (aKeys.length !== bKeys.length) return false;
  return aKeys.every(
    (key, i) => key === bKeys[i] && deepEqual(aRecord[key], bRecord[key]),
  );
}

/**
 * Assert that `events` matches the golden trace `name`, field-for-field except
 * the volatile ones {@link normalizeTrace} strips from both sides. Throws a
 * descriptive `Error` on mismatch — deliberately not `bun:test`'s `expect`, so
 * this helper works for any caller, not only inside a `bun:test` file.
 */
export function assertMatchesGolden(
  events: readonly AiRunEvent[],
  name: GoldenTraceName,
): void {
  const actual = normalizeTrace(events);
  const expected = normalizeTrace(loadGoldenTrace(name));
  if (!deepEqual(actual, expected)) {
    throw new Error(
      `Trace does not match golden "${name}".\n--- expected ---\n${JSON.stringify(
        expected,
        null,
        2,
      )}\n--- actual ---\n${JSON.stringify(actual, null, 2)}`,
    );
  }
}
