import { expect, it } from "bun:test";
import { join, resolve } from "node:path";
import {
  compareWithKnown,
  loadKnownFailures,
  parseBunTest,
  parseCargoTest,
} from "../known-failures.mjs";

const root = resolve(import.meta.dir, "../..");

const CARGO = `
     Running unittests src/lib.rs (target/debug/deps/agentkit_chat-0123456789abcdef)
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
     Running tests/session_runtime.rs (target/debug/deps/session_runtime-54736b7e205a8781)
test a_slow_one ... FAILED
test result: FAILED. 40 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.01s
     Running tests/crashes.rs (target/debug/deps/crashes-1111111111111111)
test result: FAILED. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
   Doc-tests agentkit_core
test crates/core/src/lib.rs - item (line 3) ... FAILED
test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s
`;

it("names cargo failures by binary and test, and counts unexplained failed binaries", () => {
  const parsed = parseCargoTest(CARGO);
  expect([...parsed.failures].sort()).toEqual([
    "doc:agentkit_core::crates/core/src/lib.rs - item (line 3)",
    "session_runtime::a_slow_one",
  ]);
  expect(parsed.passed).toBe(54);
  expect(parsed.binaries).toBe(4);
  expect(parsed.failedBinaries).toBe(3);
  expect(parsed.unexplained).toBe(1);
});

const BUN = `
packages/testing/tests/golden.test.ts:
(pass) golden traces > replays chat-run live and matches the golden trace [1.00ms]
(fail) golden traces > replays tool-run live and matches the golden trace [1.69ms]
scripts/tests/export-rust-fixtures.test.ts:
(fail) Rust parity fixture exporter > is deterministic [29.51ms]

 2 pass
 2 fail
`;

it("a target that failed without a test result is never explained by a known failure", () => {
  const output = `${CARGO}
error: could not compile \`agentkit-eval\` (test "memory") due to 1 previous error
error: 4 targets failed:
`;
  const parsed = parseCargoTest(output);
  expect(parsed.unexplained).toBe(3);
});

it("counts bun errors outside a named test", () => {
  expect(parseBunTest(`${BUN}\n 1 error\n`).errorCount).toBe(1);
  expect(parseBunTest(BUN).errorCount).toBe(0);
});

it("names bun failures by file and test, without the timing", () => {
  const parsed = parseBunTest(BUN);
  expect([...parsed.failures].sort()).toEqual([
    "packages/testing/tests/golden.test.ts > golden traces > replays tool-run live and matches the golden trace",
    "scripts/tests/export-rust-fixtures.test.ts > Rust parity fixture exporter > is deterministic",
  ]);
  expect(parsed.failCount).toBe(2);
  expect(parsed.files.has("packages/testing/tests/golden.test.ts")).toBe(true);
});

it("fails on a new failure and on a stale entry, but not on an intermittent one", () => {
  const entries = [
    { id: "a::listed", reason: "r" },
    { id: "a::fixed", reason: "r" },
    { id: "a::flaky", reason: "r", intermittent: true },
    { id: "b::elsewhere", reason: "r" },
  ];
  const problems = compareWithKnown(
    new Set(["a::listed", "a::new"]),
    entries,
    (id) => id.startsWith("a::"),
  );
  expect(problems).toEqual([
    "failure not on the known list: a::new",
    "stale known-failures entry, the test passes now; remove it: a::fixed",
  ]);
  expect(compareWithKnown(new Set(["a::listed"]), [entries[0]])).toEqual([]);
});

it("the checked-in list is valid and every entry gives a reason", () => {
  const list = loadKnownFailures(join(root, "scripts/known-failures.json"));
  expect(list.rust.length + list.bun.length).toBeGreaterThan(0);
});
