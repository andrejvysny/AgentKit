// Known-failures gate (hardening H0.2). The checked-in list in
// scripts/known-failures.json names every test that fails today. A test step
// passes when its only failures are on that list; any other failure fails it.
// The list may only shrink: an entry whose test now passes is reported as
// stale and fails the step until it is removed. An entry marked
// `intermittent` is never reported stale, because a pass proves nothing.
import { readFileSync } from "node:fs";

/** Load and validate the list. Keys: `rust` and `bun`, arrays of entries. */
export function loadKnownFailures(path) {
  const data = JSON.parse(readFileSync(path, "utf8"));
  for (const lane of ["rust", "bun"]) {
    if (!Array.isArray(data[lane]))
      throw new Error(`${path}: "${lane}" must be an array`);
    for (const entry of data[lane]) {
      if (
        typeof entry.id !== "string" ||
        typeof entry.reason !== "string" ||
        entry.reason === ""
      ) {
        throw new Error(
          `${path}: every ${lane} entry needs "id" and a non-empty "reason"`,
        );
      }
    }
  }
  return data;
}

// `Running tests/foo.rs (target/debug/deps/foo-0123abcd)` names the binary;
// `Doc-tests agentkit_core` opens a doc-test run.
const RUNNING = /^\s*Running .*\(.*?([^/\\]+?)-[0-9a-f]{16}(?:\.exe)?\)\s*$/;
const DOCTESTS = /^\s*Doc-tests (\S+)\s*$/;
const FAILED_TEST = /^test (.+) \.\.\. FAILED\s*$/;

/**
 * Parse `cargo test` output. Each failing test gets the id
 * `<binary>::<test path>`, where `<binary>` is the test binary's file stem
 * without its hash (`doc:<crate>` for doc-tests).
 */
export function parseCargoTest(output) {
  const failures = new Set();
  let binary = "unknown";
  let passed = 0;
  let binaries = 0;
  let failedBinaries = 0;
  let namedInBinary = 0;
  let unexplained = 0;
  for (const line of output.split("\n")) {
    const running = line.match(RUNNING);
    const doc = line.match(DOCTESTS);
    if (running || doc) {
      binary = running ? running[1] : `doc:${doc[1]}`;
      namedInBinary = 0;
      continue;
    }
    const failed = line.match(FAILED_TEST);
    if (failed) {
      failures.add(`${binary}::${failed[1]}`);
      namedInBinary += 1;
      continue;
    }
    const ok = line.match(/^test result: ok\. (\d+) passed/);
    if (ok) {
      passed += Number(ok[1]);
      binaries += 1;
      continue;
    }
    const bad = line.match(/^test result: FAILED\. (\d+) passed/);
    if (bad) {
      passed += Number(bad[1]);
      binaries += 1;
      failedBinaries += 1;
      if (namedInBinary === 0) unexplained += 1;
    }
  }
  // Cargo lists every failed target at the end; a target that never produced
  // a result (it did not compile, or its binary crashed before reporting) is
  // counted there but has no `test result` line.
  const listed = output.match(/^error: (\d+) targets? failed:/m);
  const targetsFailed = listed ? Number(listed[1]) : failedBinaries;
  unexplained += Math.max(0, targetsFailed - failedBinaries);
  if (/^error(\[E\d+\])?: could not compile/m.test(output)) unexplained += 1;
  // A failure that could not be tied to its binary cannot be matched safely.
  for (const id of failures) if (id.startsWith("unknown::")) unexplained += 1;
  return { failures, passed, binaries, failedBinaries, unexplained };
}

// Bun prints the file as a header line ending in `:`, then `(fail) <name> [<time>]`.
const BUN_FILE = /^(\S+\.(?:test|spec)\.(?:ts|tsx|js|mjs|jsx)):\s*$/;
const BUN_FAIL = /^\(fail\) (.+?)(?: \[[\d.]+m?s\])?\s*$/;

/** Parse `bun test` output. Each failing test gets the id `<file> > <name>`. */
export function parseBunTest(output) {
  const failures = new Set();
  const files = new Set();
  let file = "unknown";
  for (const line of output.split("\n")) {
    const header = line.match(BUN_FILE);
    if (header) {
      file = header[1];
      files.add(file);
      continue;
    }
    const failed = line.match(BUN_FAIL);
    if (failed) failures.add(`${file} > ${failed[1]}`);
  }
  const summary = output.match(/^\s*(\d+) fail\s*$/m);
  // Errors outside a test (a file that fails to load, an unhandled error
  // between tests) are reported as `N error(s)` and never as a named test.
  const errors = output.match(/^\s*(\d+) errors?\s*$/m);
  return {
    failures,
    files,
    failCount: summary ? Number(summary[1]) : failures.size,
    errorCount: errors ? Number(errors[1]) : 0,
  };
}

/**
 * Compare found failures with the list. `inScope(id)` says whether this run
 * executed the listed test, so a subset run does not call other entries stale.
 */
export function compareWithKnown(found, entries, inScope = () => true) {
  const known = new Map(entries.map((entry) => [entry.id, entry]));
  const problems = [];
  for (const id of [...found].sort()) {
    if (!known.has(id)) problems.push(`failure not on the known list: ${id}`);
  }
  for (const entry of entries) {
    if (entry.intermittent || found.has(entry.id) || !inScope(entry.id))
      continue;
    problems.push(
      `stale known-failures entry, the test passes now; remove it: ${entry.id}`,
    );
  }
  return problems;
}
