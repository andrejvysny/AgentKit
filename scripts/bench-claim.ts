#!/usr/bin/env bun
/**
 * Benchmarks `SqliteAssistantStore.tasks.claimNext` latency under
 * representative queue shapes, against a temp-file (not `:memory:`) db —
 * `:memory:` would skip the WAL/page-cache behavior a real deployment pays
 * for. NOT run in CI (no dev-hardware baseline to compare a CI runner
 * against) — run manually:
 *
 *   bun scripts/bench-claim.ts
 *
 * BUDGET: p95 < 5ms per claim at 5k queued tasks, on dev hardware. This is a
 * budget, not a hard SLA — CI does not gate on it. Each shape below prints its
 * own PASS/FAIL against this same ceiling; shapes (a)/(b) are what the budget
 * was written for (both are 5k queued), shapes (c)/(d) are adversarial
 * variants checked against the same number for extra confidence, not because
 * "5k queued" describes them.
 *
 * WHY THIS QUERY CAN BE SLOW — read `selectClaimCandidates` in
 * `internal/reference-adapters/src/sqlite/sqlite-assistant-store.ts` first.
 * Two things make it non-obvious that claimNext stays fast as the queue
 * grows:
 *
 *   1. It returns EVERY matching candidate, not `LIMIT 1` — claimNext has to
 *      walk past a row whose dependency is still in flight (or just failed,
 *      which it settles inline) to reach the next one, so the store cannot
 *      stop at the first match. A queue where the claimable head is buried
 *      under gated rows makes one claim as expensive as scanning the whole
 *      buried prefix.
 *   2. The ORDER BY is a computed aging expression, not a bare column, so
 *      even the plain case (no dependencies at all) cannot satisfy the sort
 *      from `idx_tasks_claim` — SQLite must materialize the matching set and
 *      sort it with a temp b-tree. `idx_tasks_claim` narrows what rows are
 *      *read* (via `status`/`available_at`); it does not narrow how many are
 *      *sorted*. That index also has no `kind` column by design (see the
 *      comment beside it in `schema.ts`), so a `kind IN (...)` filter is
 *      applied after the index range, not by it.
 *
 * Both are load-bearing, documented tradeoffs in the source, not bugs — this
 * script exists to keep them honest as the reference adapter evolves, not to
 * argue they should change.
 *
 * SHAPES:
 *   (a) 5k plain queued tasks, 1 scope, no dependencies. The baseline the
 *       budget is calibrated against: every claim call materializes and
 *       sorts ~5k rows before returning the first one.
 *   (b) 5k tasks / 500 scopes, with a realistic slice (50 scopes, 10%)
 *       reported busy — exercises the `scope_id NOT IN (...)` filter and a
 *       `kind IN (...)` filter together, the shape closest to a live
 *       multi-tenant worker's actual call.
 *   (c) 1k tasks where the top-priority 100 are dependency-gated: 50 "doomed"
 *       (their one dependency is dead-lettered — settled to `failed` inline,
 *       on the FIRST claim that reaches them, then gone from the queued set)
 *       and 50 "blocked" (their one dependency is left queued forever, via a
 *       kind this run never claims — a PERMANENT tax on every claim for the
 *       rest of the run). Elevated priority keeps all 100 sorted ahead of the
 *       900 plain tasks, so every claim walks the gated wall first.
 *   (d) Deep dependency chains: 5 independent chains of depth 100 (500 tasks
 *       total), each task depending on exactly its predecessor, with each
 *       chain's ROOT pre-failed via `markDeadLettered` before timing starts.
 *       Elevated priority + earliest `enqueuedAt` keeps the (initially ~495
 *       queued, doomed-but-not-yet-settled) chain tasks sorted ahead of a
 *       2000-task background pool. Whether a doom cascade down one chain
 *       settles several levels per claim or one level per claim (the port
 *       doc's "successive claims" language) is exactly what this shape
 *       measures rather than assumes — see the settled-count line it prints.
 *
 * Each shape: 200 timed `claimNext` calls (`performance.now()`), p50/p95 over
 * those samples, then `EXPLAIN QUERY PLAN` for the equivalent candidate
 * query — reconstructed here (bun:sqlite has no way to ask a live
 * `SqliteAssistantStore` for its own query plan) rather than exported from
 * the store, so it MUST be kept byte-for-byte in sync with
 * `selectClaimCandidates` in sqlite-assistant-store.ts; a diff between what
 * this prints and that function is this script drifting, not a real
 * regression.
 */
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteAssistantStore } from "../internal/reference-adapters/src/index.js";
import type {
  AssistantStore,
  ClaimNextInput,
} from "../packages/host/src/index.js";

const BUDGET_P95_MS = 5;
const CLAIMS_PER_SHAPE = 200;
const KIND = "bench.work";
const BLOCKER_KIND = "bench.blocker"; // never in `kinds`, so never claimed

function percentile(sortedAsc: number[], p: number): number {
  if (sortedAsc.length === 0) return Number.NaN;
  const rank = Math.ceil((p / 100) * sortedAsc.length) - 1;
  const clamped = Math.min(Math.max(rank, 0), sortedAsc.length - 1);
  return sortedAsc[clamped] as number;
}

function stats(latenciesMs: number[]): {
  p50: number;
  p95: number;
  max: number;
} {
  const sorted = [...latenciesMs].sort((a, b) => a - b);
  return {
    p50: percentile(sorted, 50),
    p95: percentile(sorted, 95),
    max: sorted[sorted.length - 1] ?? Number.NaN,
  };
}

function fmt(ms: number): string {
  return `${ms.toFixed(3)}ms`;
}

/** One task per call, batched into a single transaction — see `store.transaction`'s flattening note. */
async function seedTasks(
  store: AssistantStore,
  count: number,
  build: (i: number) => {
    taskId: string;
    kind: string;
    scopeId: string;
    priority?: number;
    dependsOn?: string[];
  },
): Promise<void> {
  await store.transaction(async (tx) => {
    for (let i = 0; i < count; i++) {
      const spec = build(i);
      await tx.tasks.createTask({
        taskId: spec.taskId,
        kind: spec.kind,
        scopeId: spec.scopeId,
        payload: { i },
        ...(spec.priority === undefined ? {} : { priority: spec.priority }),
        ...(spec.dependsOn === undefined ? {} : { dependsOn: spec.dependsOn }),
      });
    }
  });
}

/**
 * Times `CLAIMS_PER_SHAPE` calls to `claimNext`. Requires the shape to leave
 * at least that many claimable tasks reachable, or this throws instead of
 * silently reporting stats over fewer samples than the spec promises.
 */
async function timeClaims(
  store: AssistantStore,
  input: Omit<ClaimNextInput, "now">,
): Promise<{ latenciesMs: number[] }> {
  const latenciesMs: number[] = [];
  for (let i = 0; i < CLAIMS_PER_SHAPE; i++) {
    const start = performance.now();
    const claimed = await store.tasks.claimNext({ ...input, now: new Date() });
    const elapsed = performance.now() - start;
    if (claimed === null) {
      throw new Error(
        `claimNext returned null on call ${i + 1}/${CLAIMS_PER_SHAPE} — the shape did not leave enough claimable work; widen the ready pool.`,
      );
    }
    latenciesMs.push(elapsed);
  }
  return { latenciesMs };
}

/**
 * Mirrors `selectClaimCandidates` in sqlite-assistant-store.ts EXACTLY — see
 * the module doc on why this can't just call the private method.
 */
function candidateQuerySql(
  scopesBusyCount: number,
  kindsCount: number | undefined,
): { sql: string; params: Record<string, unknown> } {
  let sql = `SELECT * FROM tasks WHERE status = 'queued' AND available_at <= $now`;
  const params: Record<string, unknown> = {
    $now: new Date().toISOString(),
    $agingIntervalMs: 30_000,
    $agingBonus: 0,
    $agingMaxBonus: 0,
  };
  if (scopesBusyCount > 0) {
    const placeholders = Array.from(
      { length: scopesBusyCount },
      (_, i) => `$busy${i}`,
    ).join(", ");
    sql += ` AND scope_id NOT IN (${placeholders})`;
    for (let i = 0; i < scopesBusyCount; i++)
      params[`$busy${i}`] = `scope-${i}`;
  }
  if (kindsCount !== undefined) {
    const placeholders = Array.from(
      { length: kindsCount },
      (_, i) => `$kind${i}`,
    ).join(", ");
    sql += ` AND kind IN (${placeholders})`;
    for (let i = 0; i < kindsCount; i++) params[`$kind${i}`] = KIND;
  }
  sql += ` ORDER BY (priority + MIN($agingMaxBonus,
             CAST(MAX(0, (julianday($now) - julianday(enqueued_at)) * 86400000.0)
                  / $agingIntervalMs AS INTEGER) * $agingBonus)) DESC,
           enqueued_at ASC, rowid ASC`;
  return { sql, params };
}

function printExplainQueryPlan(dbPath: string): void {
  const db = new Database(dbPath, { readonly: true });
  try {
    const { sql, params } = candidateQuerySql(50, 1);
    console.log(
      "\nEXPLAIN QUERY PLAN for the candidate query (50 busy scopes, 1 kind filter):",
    );
    console.log(`  ${sql.replace(/\s+/g, " ").trim()}`);
    const rows = db.query(`EXPLAIN QUERY PLAN ${sql}`).all(params) as Array<{
      detail: string;
    }>;
    for (const row of rows) console.log(`    ${row.detail}`);
  } finally {
    db.close();
  }
}

function report(latenciesMs: number[], budgetMs: number): boolean {
  const { p50, p95, max } = stats(latenciesMs);
  const pass = p95 < budgetMs;
  console.log(
    `  p50=${fmt(p50)}  p95=${fmt(p95)}  max=${fmt(max)}  ` +
      `(n=${latenciesMs.length})  budget p95<${budgetMs}ms: ${pass ? "PASS" : "FAIL"}`,
  );
  return pass;
}

async function benchShapeA(store: AssistantStore): Promise<boolean> {
  console.log("\n(a) 5k plain queued tasks, 1 scope");
  await seedTasks(store, 5000, (i) => ({
    taskId: `a-${i}`,
    kind: KIND,
    scopeId: "scope-a",
  }));
  const { latenciesMs } = await timeClaims(store, {
    ownerId: "bench",
    scopesBusy: [],
    kinds: [KIND],
  });
  return report(latenciesMs, BUDGET_P95_MS);
}

async function benchShapeB(store: AssistantStore): Promise<boolean> {
  console.log("\n(b) 5k tasks / 500 scopes, 50 busy");
  const scopeCount = 500;
  await seedTasks(store, 5000, (i) => ({
    taskId: `b-${i}`,
    kind: KIND,
    scopeId: `scope-${i % scopeCount}`,
  }));
  const scopesBusy = Array.from({ length: 50 }, (_, i) => `scope-${i}`);
  const { latenciesMs } = await timeClaims(store, {
    ownerId: "bench",
    scopesBusy,
    kinds: [KIND],
  });
  return report(latenciesMs, BUDGET_P95_MS);
}

async function benchShapeC(store: AssistantStore): Promise<boolean> {
  console.log(
    "\n(c) 1k tasks, top-100 dependency-gated (50 doomed + 50 blocked)",
  );
  const GATED_PRIORITY = 100;
  // Dependencies for the doomed half: created queued, then dead-lettered —
  // `evaluateTaskDependencies` treats a dead-lettered dependency as doomed
  // regardless of its status field.
  await seedTasks(store, 50, (i) => ({
    taskId: `c-doom-dep-${i}`,
    kind: BLOCKER_KIND,
    scopeId: "scope-c-dep",
  }));
  for (let i = 0; i < 50; i++) {
    await store.tasks.markDeadLettered(`c-doom-dep-${i}`, "bench: pre-doomed");
  }
  // Dependencies for the blocked half: left queued forever (never in
  // `kinds`, so this run's claimNext calls can never reach and resolve them).
  await seedTasks(store, 50, (i) => ({
    taskId: `c-block-dep-${i}`,
    kind: BLOCKER_KIND,
    scopeId: "scope-c-dep",
  }));
  await seedTasks(store, 50, (i) => ({
    taskId: `c-doomed-${i}`,
    kind: KIND,
    scopeId: "scope-c",
    priority: GATED_PRIORITY,
    dependsOn: [`c-doom-dep-${i}`],
  }));
  await seedTasks(store, 50, (i) => ({
    taskId: `c-blocked-${i}`,
    kind: KIND,
    scopeId: "scope-c",
    priority: GATED_PRIORITY,
    dependsOn: [`c-block-dep-${i}`],
  }));
  await seedTasks(store, 900, (i) => ({
    taskId: `c-plain-${i}`,
    kind: KIND,
    scopeId: "scope-c",
  }));
  const { latenciesMs } = await timeClaims(store, {
    ownerId: "bench",
    scopesBusy: [],
    kinds: [KIND],
  });
  return report(latenciesMs, BUDGET_P95_MS);
}

async function benchShapeD(store: AssistantStore): Promise<boolean> {
  console.log("\n(d) 5 dependency chains, depth 100, roots pre-failed");
  const CHAIN_COUNT = 5;
  const CHAIN_DEPTH = 100;
  const CHAIN_PRIORITY = 100;
  for (let c = 0; c < CHAIN_COUNT; c++) {
    await seedTasks(store, CHAIN_DEPTH, (i) => ({
      taskId: `d-chain${c}-${i}`,
      kind: KIND,
      scopeId: `scope-d-chain${c}`,
      priority: CHAIN_PRIORITY,
      ...(i === 0 ? {} : { dependsOn: [`d-chain${c}-${i - 1}`] }),
    }));
  }
  // Roots pre-failed AFTER creation (dependsOn requires the id to already
  // exist at write time, so this can only happen once every chain is built).
  for (let c = 0; c < CHAIN_COUNT; c++) {
    await store.tasks.markDeadLettered(
      `d-chain${c}-0`,
      "bench: pre-failed root",
    );
  }
  await seedTasks(store, 2000, (i) => ({
    taskId: `d-bg-${i}`,
    kind: KIND,
    scopeId: `scope-d-bg-${i % 100}`,
  }));
  const before = CHAIN_COUNT * (CHAIN_DEPTH - 1); // queued dependents, roots excluded
  const { latenciesMs } = await timeClaims(store, {
    ownerId: "bench",
    scopesBusy: [],
    kinds: [KIND],
  });
  let stillQueued = 0;
  for (let c = 0; c < CHAIN_COUNT; c++) {
    for (let i = 1; i < CHAIN_DEPTH; i++) {
      const t = await store.tasks.getTask(`d-chain${c}-${i}`);
      if (t?.status === "queued") stillQueued += 1;
    }
  }
  console.log(
    `  doom cascade: ${before - stillQueued}/${before} chain dependents settled across ${CLAIMS_PER_SHAPE} claims (${stillQueued} still queued)`,
  );
  return report(latenciesMs, BUDGET_P95_MS);
}

async function main(): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-bench-claim-"));
  const path = join(dir, "bench.sqlite");
  console.log(`db: ${path}`);
  console.log(
    `budget: p95 < ${BUDGET_P95_MS}ms per claim, ${CLAIMS_PER_SHAPE} claims/shape\n`,
  );

  const results: Array<{ shape: string; pass: boolean }> = [];
  // One store per shape (fresh file each time — shapes must not interfere).
  for (const [shape, run] of [
    ["a", benchShapeA],
    ["b", benchShapeB],
    ["c", benchShapeC],
    ["d", benchShapeD],
  ] as const) {
    const shapePath = join(dir, `${shape}.sqlite`);
    const store = new SqliteAssistantStore(shapePath);
    try {
      const pass = await run(store);
      results.push({ shape, pass });
    } finally {
      store.close();
    }
  }

  // The plan printed is (b)'s — the multi-scope, multi-kind shape closest to
  // a live worker's actual call — read from that shape's own file.
  printExplainQueryPlan(join(dir, "b.sqlite"));

  rmSync(dir, { recursive: true, force: true });

  console.log("\nsummary:");
  let allPass = true;
  for (const { shape, pass } of results) {
    console.log(`  (${shape}) ${pass ? "PASS" : "FAIL"}`);
    if (!pass) allPass = false;
  }
  if (!allPass) process.exitCode = 1;
}

await main();
