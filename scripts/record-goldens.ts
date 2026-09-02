#!/usr/bin/env bun
/**
 * Regenerates the golden run-event traces committed at
 * `packages/testing/src/golden/traces/*.json`.
 *
 * `packages/testing/tests/golden.test.ts` and
 * `packages/contracts/tests/golden-validate.test.ts` check the committed
 * trace files. They are NOT regenerated on every test run — the timestamps
 * and event ids inside them are frozen history, exactly as recorded here, and
 * are committed verbatim (see `normalizeTrace` in
 * `packages/testing/src/golden/golden.ts`, which is what makes comparing a
 * live run against a frozen trace meaningful despite that).
 *
 * Regenerate deliberately, when a scenario or the run-loop's event shape
 * changes on purpose — not routinely:
 *
 *   bun scripts/record-goldens.ts
 *
 * Then run `bun test` and diff the resulting trace files before committing; a
 * golden trace changing is a signal to look at, not something to rubber-stamp.
 *
 * The scenario drivers themselves live in
 * `packages/testing/tests/golden-scenarios.ts` (shared with
 * `golden.test.ts`'s live replay, so there is exactly one place that knows
 * how to drive each scenario) — imported here by relative path, same as the
 * `@agentkit/core` import below, since this script is repo tooling, not
 * package code, and is allowed to touch `@agentkit/core` at runtime (see
 * `packages/testing`'s README/CLAUDE.md note on why `@agentkit/core` is a
 * peer dependency there, used only in that package's own tests).
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { GOLDEN_SCENARIOS } from "../packages/testing/tests/golden-scenarios.js";
import { GOLDEN_TRACE_NAMES } from "../packages/testing/src/golden/golden.js";

const TRACES_DIR = join(
  import.meta.dir,
  "..",
  "packages",
  "testing",
  "src",
  "golden",
  "traces",
);

async function main(): Promise<void> {
  mkdirSync(TRACES_DIR, { recursive: true });
  for (const name of GOLDEN_TRACE_NAMES) {
    const events = await GOLDEN_SCENARIOS[name]();
    if (events.length === 0) {
      throw new Error(`Scenario "${name}" recorded zero events.`);
    }
    const path = join(TRACES_DIR, `${name}.json`);
    writeFileSync(path, `${JSON.stringify(events, null, 2)}\n`, "utf8");
    console.log(`wrote ${path} (${events.length} events)`);
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
