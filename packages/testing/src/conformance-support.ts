// The shared vocabulary of the store-conformance suites: the harness a caller
// builds, the injected test primitives, and the two rejection assertions every
// section uses.
//
// Split out of `store-conformance.ts` when the conversation-branching section
// arrived, so a second suite module could be written against the same contract
// without either file importing the other's tests. Everything here is re-exported
// by `store-conformance.ts`, which is still the module a consumer imports.
//
// FRAMEWORK-NEUTRAL BY DESIGN, same as the suites: no `bun:test`, no runner
// globals, and every `@agentkit/host` import is `import type`.
import type { AssistantStore, Clock } from "@agentkit/host";

/**
 * What `create()` hands back for one test: a fresh, isolated store plus
 * whatever this adapter cannot promise.
 */
export interface AssistantStoreConformanceHarness {
  store: AssistantStore;
  /**
   * Capabilities this adapter does NOT provide. Absent/undefined means "fully
   * capable" — only `atomicTransactions: false` currently changes suite
   * behavior (see {@link DescribeAssistantStoreConformanceOptions}).
   */
  capabilities?: {
    /** False for an adapter whose `transaction()` cannot roll back (e.g. a plain in-memory store). */
    atomicTransactions?: boolean;
  };
  /** Releases whatever `create()` opened (a db connection, a temp file handle, ...). Synchronous — mirrors `SqliteAssistantStore.close()`. */
  close?: () => void;
}

/**
 * Minimal test-runner primitives this suite needs, injected by the caller so
 * this package never imports a specific runner.
 *
 * `expect` is typed loosely (`any` return) on purpose: every assertion below
 * uses only the near-universal Jest-style subset (`toBe`, `toEqual`,
 * `toBeNull`, `toBeDefined`, `toContain`, `toBeGreaterThan`, `.not`), and
 * giving that chain a precise type would mean importing one runner's matcher
 * types — exactly what this file exists to avoid.
 */
export interface AssistantStoreConformanceTestApi {
  describe: (name: string, fn: () => void) => void;
  it: (name: string, fn: () => void | Promise<void>) => void;
  // biome-ignore lint/suspicious/noExplicitAny: intentional, see doc comment above — a precise return type here means importing one runner's matcher types.
  expect: (value: unknown) => any;
  beforeEach?: (fn: () => void | Promise<void>) => void;
}

/** A {@link Clock} a test drives by hand. */
export interface ConformanceClock extends Clock {
  advance(ms: number): void;
}

/**
 * Construction knobs the queue tests need but `create()` cannot express.
 *
 * Aging is a function of how long a task has waited, so the only way to observe
 * it is to control the clock the store stamps `enqueuedAt` from — two tasks
 * created a millisecond apart carry the same age whatever `now` a claim passes.
 * Hence the clock is part of the tuning, not an extra.
 */
export interface ConformanceTuning {
  /** What the store stamps `enqueuedAt` / `availableAt` from. */
  clock: ConformanceClock;
  /** Aging knobs. Absent means the adapter's own defaults, i.e. aging off. */
  aging?: {
    agingIntervalMs?: number;
    agingBonus?: number;
    agingMaxBonus?: number;
  };
}

export interface DescribeAssistantStoreConformanceOptions {
  /** Adapter name, folded into every `describe` block title. */
  name: string;
  /** Builds one fresh, isolated store per test — never shared across `it()`s. */
  create: () => Promise<AssistantStoreConformanceHarness>;
  /**
   * Builds a store with an injected clock and optional priority aging. Adapters
   * that cannot be constructed that way omit it and the aging tests are skipped
   * rather than failed — the rest of the suite still grades them.
   */
  createTuned?: (
    tuning: ConformanceTuning,
  ) => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** A clock frozen at `startIso` that only moves when a test says so. */
export function createConformanceClock(startIso: string): ConformanceClock {
  let current = new Date(startIso).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms: number) => {
      current += ms;
    },
  };
}

/** Catches a rejection and asserts its `code` field — see the module doc on why `code`, not `instanceof`. */
export async function expectRejectsWithCode(
  promise: Promise<unknown>,
  expectedCode: string,
  expect: AssistantStoreConformanceTestApi["expect"],
): Promise<void> {
  let caught: { code?: string } | undefined;
  try {
    await promise;
  } catch (err) {
    caught = err as { code?: string };
  }
  expect(caught).toBeDefined();
  expect(caught?.code).toBe(expectedCode);
}

/** Catches a rejection for any reason — used for the plain-Error atomicity probe. */
export async function expectRejects(
  promise: Promise<unknown>,
  expect: AssistantStoreConformanceTestApi["expect"],
): Promise<void> {
  let threw = false;
  try {
    await promise;
  } catch {
    threw = true;
  }
  expect(threw).toBe(true);
}
