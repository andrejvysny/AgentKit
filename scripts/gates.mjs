#!/usr/bin/env node
// Full gate battery for AgentKit: Rust half then Bun/TS half, one process.
// Every step logs to .gate-logs/<name>.log; the summary and exit code are
// the gate. CI (.github/workflows/ci.yml) runs exactly this file.
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
// Usage:
//   node scripts/gates.mjs                  # everything
//   node scripts/gates.mjs --only=test,arch # a subset, given order preserved
//   node scripts/gates.mjs --skip=api       # drop steps
//   node scripts/gates.mjs --floor=0        # relax the passing-test floor
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const LOGS = join(ROOT, ".gate-logs");

// Floor is the whole-workspace passing-test count observed at the time of
// writing (1,761); it catches test binaries that stop running rather than
// fail. Raise it whenever the suite grows; never lower it silently.
const DEFAULT_TEST_FLOOR = 1700;

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
  {
    name: "native",
    cmd: ["bun", "test", "packages/native", "packages/core"],
    env: { AGENTKIT_REQUIRE_NATIVE: "1" },
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

// The test step is the only one with a quantitative contract: every test
// binary must report, nothing may fail, and the total passing count must
// not sink below the floor (a silently-missing binary reports nothing).
function auditCargoTest(output, floor) {
  const problems = [];
  let passed = 0;
  let binaries = 0;
  let failedBinaries = 0;
  for (const line of output.split("\n")) {
    const ok = line.match(/^test result: ok\. (\d+) passed/);
    if (ok) {
      passed += Number(ok[1]);
      binaries += 1;
      continue;
    }
    if (/^test result: FAILED/.test(line)) failedBinaries += 1;
  }
  if (failedBinaries > 0)
    problems.push(`${failedBinaries} test binary(ies) FAILED`);
  if (binaries === 0) problems.push("no test binaries reported");
  if (passed < floor) problems.push(`passing tests ${passed} < floor ${floor}`);
  return { problems, detail: `${passed} passed across ${binaries} binaries` };
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
  const result = spawnSync(step.cmd[0], step.cmd.slice(1), {
    cwd: join(ROOT, step.cwd ?? "."),
    encoding: "utf8",
    maxBuffer: 512 * 1024 * 1024,
    ...(step.env ? { env: { ...process.env, ...step.env } } : {}),
  });
  const out = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  writeFileSync(join(LOGS, `${step.name}.log`), out);
  const seconds = Math.round((Date.now() - start) / 1000);
  const failures = [];
  if (result.error) failures.push(`spawn failed: ${result.error.message}`);
  else if (result.status !== 0) failures.push(`exit ${result.status}`);
  if (step.audit) {
    const audit = step.audit(out, opts.floor);
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
