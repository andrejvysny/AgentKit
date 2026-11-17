#!/usr/bin/env node
// Full gate battery for AgentKit: Rust half then Bun/TS half, one process.
// Every step logs to .gate-logs/<name>.log; the summary and exit code are
// the gate. Run locally; hosted GitHub Actions are disabled.
//
// Ordering rules learned the hard way:
//   - `check:api` (api-extractor over built .d.ts) and `node-smoke.mjs`
//     (imports built dists) MUST run after `bun run build`.
//   - the `api` step needs `cargo +nightly public-api` installed and the
//     1.97.0 toolchain pinned by rust/rust-toolchain.toml.
//   - `napi` (the Rust napi addon) MUST run before `build`, `check:api` and
//     `smoke`: a clean runner has no prebuilt binding and they fail with
//     "Cannot find native binding".
//   - the `native` test step MUST run after `build` and with
//     `AGENTKIT_REQUIRE_NATIVE=1` so a missing binding fails loudly.
//
// Known failures (hardening H0.2): the Rust `test` step and the Bun test
// steps pass only when every failure is named in scripts/known-failures.json,
// and fail when a listed test passes again (unless marked intermittent). The
// list may only shrink.
//
// Usage:
//   node scripts/gates.mjs                  # everything
//   node scripts/gates.mjs --only=test,arch # a subset, given order preserved
//   node scripts/gates.mjs --skip=api       # drop steps
//   node scripts/gates.mjs --floor=0        # relax the passing-test floor
import { spawnSync } from "node:child_process";
import { closeSync, mkdirSync, openSync, readFileSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import {
  bunScope,
  compareWithKnown,
  loadKnownFailures,
  parseBunTest,
  parseCargoTest,
} from "./known-failures.mjs";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const LOGS = join(ROOT, ".gate-logs");

// Floor is the whole-workspace passing-test count observed at the time of
// writing (1,761); it catches test binaries that stop running rather than
// fail. Raise it whenever the suite grows; never lower it silently.
const DEFAULT_TEST_FLOOR = 1700;
// Tests that fail today, by name (H0.2). A test step passes when its only
// failures are on this list; see scripts/known-failures.mjs.
const KNOWN = loadKnownFailures(join(ROOT, "scripts", "known-failures.json"));

const RUST = "rust";
const steps = [
  { name: "fmt", cmd: ["cargo", "fmt", "--all", "--check"], cwd: RUST },
  {
    name: "clippy",
    cmd: [
      "cargo",
      "clippy",
      "--locked",
      "--workspace",
      "--all-targets",
      "--all-features",
      "--",
      "-D",
      "warnings",
    ],
    cwd: RUST,
  },
  {
    name: "test",
    cmd: [
      "cargo",
      "test",
      "--locked",
      "--workspace",
      "--all-features",
      "--no-fail-fast",
    ],
    cwd: RUST,
    audit: auditCargoTest,
  },
  { name: "machete", cmd: ["cargo", "machete"], cwd: RUST },
  {
    name: "deny",
    cmd: ["cargo", "deny", "check", "licenses", "bans", "advisories"],
    cwd: RUST,
  },
  { name: "arch", cmd: ["node", "rust/scripts/check-arch.mjs"] },
  { name: "api", cmd: ["node", "rust/scripts/public-api.mjs"] },
  { name: "buildfix", cmd: ["bun", "run", "build:fixture"] },
  {
    name: "clientreact",
    cmd: ["bun", "test", "packages/client/tests", "packages/react/tests"],
    audit: auditBunTest(["packages/client/tests", "packages/react/tests"]),
  },
  { name: "contracts", cmd: ["bun", "run", "contracts:check"] },
  { name: "typecheck", cmd: ["bun", "run", "typecheck"] },
  { name: "lint", cmd: ["bun", "run", "lint"] },
  // The napi addon is what the root `build` step's tsc-only pass omits;
  // build it first so checkapi/smoke find a real binding.
  {
    name: "napi",
    cmd: ["bun", "run", "--cwd", "packages/native", "build:native"],
  },
  { name: "build", cmd: ["bun", "run", "build"] },
  { name: "umbrella", cmd: ["bun", "run", "build:umbrella"] },
  {
    name: "native",
    cmd: [
      "bun",
      "test",
      "packages/native",
      "packages/core",
      "--timeout",
      "60000",
    ],
    env: { AGENTKIT_REQUIRE_NATIVE: "1" },
    audit: auditBunTest(["packages/native", "packages/core"]),
  },
  // The whole Bun suite, examples and scripts included, against the list.
  {
    name: "buntest",
    cmd: ["bun", "test", "--timeout", "60000"],
    env: { AGENTKIT_REQUIRE_NATIVE: "1" },
    audit: auditBunTest([]),
  },
  // Keep these last: both consume what `build` emits.
  { name: "checkapi", cmd: ["bun", "run", "check:api"] },
  { name: "smoke", cmd: ["node", "scripts/node-smoke.mjs"] },
];

function parseArgs(argv) {
  const opts = { only: null, skip: new Set(), floor: DEFAULT_TEST_FLOOR };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--only") opts.only = argv[++i]?.split(",") ?? [];
    else if (arg.startsWith("--only=")) opts.only = arg.slice(7).split(",");
    else if (arg === "--skip")
      for (const s of (argv[++i] ?? "").split(",")) opts.skip.add(s);
    else if (arg.startsWith("--skip="))
      for (const s of arg.slice(7).split(",")) opts.skip.add(s);
    else if (arg.startsWith("--floor=")) opts.floor = Number(arg.slice(8));
    else {
      console.error(`gates: unknown argument ${arg}`);
      process.exit(2);
    }
  }
  return opts;
}

// The Rust test step has a quantitative contract: every test binary must
// report, every failure must be on the known list (and no listed test may
// pass again unnoticed), and the total passing count must not sink below the
// floor (a silently-missing binary reports nothing). A failed binary with no
// named failing test (a crash or a compile error) always fails the step.
function auditCargoTest(output, floor) {
  const parsed = parseCargoTest(output);
  const problems = compareWithKnown(parsed.failures, KNOWN.rust);
  if (parsed.unexplained > 0)
    problems.push(
      `${parsed.unexplained} test binary(ies) FAILED without a named test`,
    );
  if (parsed.binaries === 0) problems.push("no test binaries reported");
  if (parsed.passed < floor)
    problems.push(`passing tests ${parsed.passed} < floor ${floor}`);
  return {
    problems,
    // A non-zero exit is explained when every failure is a known one.
    explainsExit: problems.length === 0 && parsed.failures.size > 0,
    detail: `${parsed.passed} passed across ${parsed.binaries} binaries; ${parsed.failures.size} known failure(s)`,
  };
}

// Bun steps: every failure must be on the known list. Entries outside the
// paths a step runs are out of scope, so a subset run never calls them stale.
function auditBunTest(paths) {
  return (output) => auditBunOutput(output, bunScope(paths));
}

function auditBunOutput(output, inScope) {
  const parsed = parseBunTest(output);
  const problems = compareWithKnown(parsed.failures, KNOWN.bun, inScope);
  if (parsed.errorCount > 0)
    problems.push(
      `bun reported ${parsed.errorCount} error(s) outside a named test`,
    );
  if (parsed.failCount !== parsed.failures.size)
    problems.push(
      `bun reported ${parsed.failCount} failure(s) but ${parsed.failures.size} were named`,
    );
  return {
    problems,
    explainsExit: problems.length === 0 && parsed.failures.size > 0,
    detail: `${parsed.failures.size} known failure(s)`,
  };
}

const opts = parseArgs(process.argv.slice(2));
rmSync(LOGS, { recursive: true, force: true });
mkdirSync(LOGS, { recursive: true });

const selected = steps.filter(
  (s) =>
    (opts.only ? opts.only.includes(s.name) : true) && !opts.skip.has(s.name),
);
if (selected.length === 0) {
  console.error("gates: no steps selected");
  process.exit(2);
}

let overall = 0;
const results = [];
for (const step of selected) {
  const start = Date.now();
  // stdout and stderr share one file descriptor so the log keeps their
  // order: cargo prints `Running <binary>` on stderr and the test results on
  // stdout, and the known-failures audit needs both in sequence.
  const logPath = join(LOGS, `${step.name}.log`);
  const log = openSync(logPath, "w");
  const result = spawnSync(step.cmd[0], step.cmd.slice(1), {
    cwd: join(ROOT, step.cwd ?? "."),
    stdio: ["ignore", log, log],
    ...(step.env ? { env: { ...process.env, ...step.env } } : {}),
  });
  closeSync(log);
  const out = readFileSync(logPath, "utf8");
  const seconds = Math.round((Date.now() - start) / 1000);
  const failures = [];
  const audit = step.audit ? step.audit(out, opts.floor) : null;
  if (result.error) failures.push(`spawn failed: ${result.error.message}`);
  else if (result.status !== 0 && !audit?.explainsExit)
    failures.push(`exit ${result.status}`);
  if (audit) {
    failures.push(...audit.problems);
    if (audit.detail) console.log(`gates: ${step.name}: ${audit.detail}`);
  }
  const status = failures.length === 0 ? "PASS" : "FAIL";
  if (failures.length > 0) {
    overall = 1;
    const tail = out.trimEnd().split("\n").slice(-20).join("\n");
    console.log(
      `FAIL ${step.name} (${seconds}s): ${failures.join("; ")} — .gate-logs/${step.name}.log`,
    );
    console.log(tail);
  } else {
    console.log(`PASS ${step.name} (${seconds}s)`);
  }
  results.push({ status, name: step.name });
}

console.log(
  `OVERALL=${overall} (${results.filter((r) => r.status === "PASS").length}/${results.length} passed)`,
);
process.exit(overall);
