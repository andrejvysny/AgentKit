#!/usr/bin/env bun
// Sandbox runtime evidence report generator (M7, spec §15.2/§15.4).
//
// Runs the machine-checkable L0/L1 evidence for the T01-T24 acceptance
// matrix and records hardware-gated L2/L3/L4 rows TRUTHFULLY: a row is
// PASS only if the exact command ran and cargo reported success; hardware
// rows print NOT RUN with the concrete missing prerequisite, never a fake
// pass (S19). Emits markdown + machine-readable JSON into docs/baseline/.
//
// Usage: bun scripts/sandbox-evidence.mjs [--out docs/baseline]
// Env:   CARGO_TARGET_DIR (default <repo>/rust/target)
//        AGENTKIT_EVIDENCE_QUICK=1  skip real-hardware probe runs (records
//                                   them as NOT RUN (operator skip)).

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { accessSync, constants } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const rustDir = join(repoRoot, "rust");
const targetDir = process.env.CARGO_TARGET_DIR ?? join(rustDir, "target");
const quick = process.env.AGENTKIT_EVIDENCE_QUICK === "1";
const outDir = (() => {
  const i = process.argv.indexOf("--out");
  return i > 0 && process.argv[i + 1]
    ? resolve(process.argv[i + 1])
    : join(repoRoot, "docs", "baseline");
})();

function run(cwd, cmd, args, { timeoutMs = 25 * 60_000, env = {} } = {}) {
  const r = spawnSync(cmd, args, {
    cwd,
    encoding: "utf8",
    timeout: timeoutMs,
    env: { ...process.env, CARGO_TARGET_DIR: targetDir, ...env },
    maxBuffer: 64 * 1024 * 1024,
  });
  const out = (r.stdout ?? "") + (r.stderr ?? "");
  return {
    command: `${cmd} ${args.join(" ")}`,
    exit: r.status ?? -1,
    timedOut: r.error?.code === "ETIMEDOUT",
    out,
  };
}

function parseCargoTest(res) {
  const failed = (res.out.match(/test result: FAILED/g) ?? []).length > 0;
  const panicked = /panicked at/.test(res.out);
  const notRunLines = res.out
    .split("\n")
    .filter((l) => l.includes("NOT RUN"))
    .map((l) => l.trim());
  const passed =
    (res.out.match(/test result: ok\. (\d+) passed/g) ?? [])
      .map((m) => Number(m.match(/(\d+) passed/)[1]))
      .reduce((a, b) => a + b, 0);
  let status;
  if (res.timedOut) status = "FAIL (timeout)";
  else if (failed || panicked) status = "FAIL";
  else if (res.exit !== 0 && passed === 0) status = "FAIL (command error)";
  else if (notRunLines.length > 0 && passed === 0) status = "NOT RUN";
  else if (notRunLines.length > 0) status = "PARTIAL (some NOT RUN)";
  else if (passed === 0 && /running 0 tests/.test(res.out)) status = "NOT RUN (no tests selected)";
  else status = "PASS";
  return { status, passed, notRun: notRunLines };
}

const cargoTest = (pkg, extraArgs = [], filter = []) => {
  const r = run(rustDir, "cargo", ["test", "-p", pkg, ...extraArgs, ...filter, "--", "--nocapture"]);
  return { ...parseCargoTest(r), command: r.command, exit: r.exit };
};

// Hardware-gated harnesses: run only when their prerequisites are actually
// present; otherwise record NOT RUN with the exact missing prerequisite
// (S19 -- a vacuous "running 0 tests" is never reported as PASS).
const GATES = {
  "agentkit-sandbox-worker:real_vm": () => {
    const missing = [];
    if (process.env.AGENTKIT_REAL_VM !== "1") missing.push("AGENTKIT_REAL_VM=1 unset");
    if (!probes.kvm.startsWith("available")) missing.push(`/dev/kvm: ${probes.kvm}`);
    if (probes.firecracker === "not on PATH" && !process.env.AGENTKIT_FC_BIN)
      missing.push("firecracker not on PATH (or AGENTKIT_FC_BIN unset)");
    if (probes.jailer === "not on PATH" && !process.env.AGENTKIT_JAILER_BIN)
      missing.push("jailer not on PATH (or AGENTKIT_JAILER_BIN unset)");
    return {
      missing,
      command:
        "cd rust && cargo test -p agentkit-sandbox-worker --features firecracker --test real_vm",
    };
  },
  "agentkit-sandbox-worker-apple:parity-real-vz": () => ({
    missing:
      process.platform === "darwin"
        ? []
        : [`macOS host required (platform=${process.platform}); signed VZ helper + entitlements needed`],
    command:
      "cd rust && cargo test -p agentkit-sandbox-worker-apple --test parity -- --ignored",
  }),
  "agentkit-toolpacks-coding:sandbox_escape": () => ({
    missing:
      process.platform === "darwin"
        ? []
        : [`macOS-only compile-time gated suite (target_os=macos); platform=${process.platform}`],
    command: "cd rust && cargo test -p agentkit-toolpacks-coding --test sandbox_escape",
  }),
};

const gatedRunEntry = ({ pkg, gated }, quick) => {
  const g = GATES[`${pkg}:${gated}`]();
  if (quick) {
    return {
      command: g.command,
      status: "NOT RUN",
      passed: 0,
      notRun: [`operator skip (AGENTKIT_EVIDENCE_QUICK=1); platform=${process.platform}, kvm=${probes.kvm}, firecracker=${probes.firecracker}`],
      exit: 0,
    };
  }
  if (g.missing.length > 0) {
    return { command: g.command, status: "NOT RUN", passed: 0, notRun: [g.missing.join("; ")], exit: 0 };
  }
  if (gated === "parity-real-vz") {
    const res = run(rustDir, "cargo", ["test", "-p", pkg, "--test", "parity", "--", "--ignored", "--nocapture"]);
    return { ...parseCargoTest(res), command: res.command, exit: res.exit };
  }
  if (gated === "real_vm") {
    const res = run(rustDir, "cargo", ["test", "-p", pkg, "--features", "firecracker", "--test", "real_vm", "--", "--nocapture"]);
    return { ...parseCargoTest(res), command: res.command, exit: res.exit };
  }
  return cargoTest(pkg, ["--test", gated], []);
};

// ---- hardware probes (never fabricated) -----------------------------------
const probes = {};
probes.platform = process.platform;
probes.uname = run(repoRoot, "uname", ["-a"]).out.trim();
probes.rustc = run(rustDir, "rustc", ["--version"]).out.trim();
probes.cargo = run(rustDir, "cargo", ["--version"]).out.trim();
try {
  accessSync("/dev/kvm", constants.R_OK | constants.W_OK);
  probes.kvm = "available (rw for this uid)";
} catch {
  probes.kvm = "unavailable (missing or no rw access for uid " + process.getuid?.() + ")";
}
const fc = spawnSync("which", ["firecracker"], { encoding: "utf8" });
probes.firecracker = fc.status === 0 ? fc.stdout.trim() : "not on PATH";
const jailer = spawnSync("which", ["jailer"], { encoding: "utf8" });
probes.jailer = jailer.status === 0 ? jailer.stdout.trim() : "not on PATH";
probes.vzHelper =
  process.platform === "darwin"
    ? process.env.AGENTKIT_VZ_HELPER_BIN ?? "AGENTKIT_VZ_HELPER_BIN unset"
    : "n/a (Linux host)";

// ---- T01-T24 evidence matrix ----------------------------------------------
// Each entry names the EXACT harnesses that must run for the row; hardware
// entries run their self-gating harnesses, which print NOT RUN themselves.
const realVM = { pkg: "agentkit-sandbox-worker", gated: "real_vm" };
const realVZ = { pkg: "agentkit-sandbox-worker-apple", gated: "parity-real-vz" };
const appleParityL1 = ["agentkit-sandbox-worker-apple", ["--test", "parity"]]; // fake-helper L1 contract
const sandboxEscape = { pkg: "agentkit-toolpacks-coding", gated: "sandbox_escape" };

const MATRIX = [
  ["T01", "cross-tenant denial incl. guessed ids", [
    ["agentkit-sandbox-worker", ["--test", "tenant_denial"]],
    ["agentkit-sandbox-worker", ["--test", "fuzz_dispatch"]],
    realVM, realVZ,
  ]],
  ["T02", "no VM/hypervisor => IsolationUnavailable, never a fallback", [
    ["agentkit-sandbox-worker-fc", ["--lib"], ["probe::tests::probe_fails_closed_without_binaries"]],
    realVM,
  ]],
  ["T03", "guest workspace isolation from host/other tenants", [realVM, realVZ]],
  ["T04", "guest network absent; no DNS/IPv4/IPv6/private-IP", [realVM, realVZ]],
  ["T05", "approved packages via structured broker only", [
    ["agentkit-sandbox-worker", ["--test", "broker_flow"]],
    realVM,
  ]],
  ["T06", "unstructured/secret-bearing registry requests denied", [
    ["agentkit-sandbox-worker", ["--test", "broker_flow"]],
    realVM,
  ]],
  ["T07", "OCI hostile tar shapes refused", [
    ["agentkit-sandbox-worker", ["--test", "oci_import"]],
    ["agentkit-sandbox-guest", ["--test", "fsops_hostile"]],
    realVM,
  ]],
  ["T08", "OCI digest-pinned conversion + boot validation", [
    ["agentkit-sandbox-worker", ["--test", "oci_import"]],
    realVM,
  ]],
  ["T09", "exec semantics: outputs, exits, timeouts, cancellation", [
    ["agentkit-sandbox-worker", ["--test", "exec_flow"]],
    ["agentkit-sandbox-guest", ["--test", "exec_supervisor"]],
    realVM,
  ]],
  ["T10", "descendants killed or VM quarantined", [
    ["agentkit-sandbox-guest", ["--test", "exec_supervisor"]],
    realVM,
  ]],
  ["T11", "crash at dispatch/receipt boundaries; no false completed", [
    ["agentkit-sandbox-worker", ["--test", "fault_injection"]],
    ["agentkit-sandbox-worker", ["--test", "journal_replay"]],
    ["agentkit-sandbox-host", ["--test", "binding_reconcile"]],
    realVM,
  ]],
  ["T12", "resource exhaustion: quotas enforced", [
    ["agentkit-sandbox-worker", ["--test", "quotas"]],
    realVM,
  ]],
  ["T13", "import staging: traversal/symlink/oversize/cross-tenant denied", [
    ["agentkit-sandbox-worker", ["--test", "staging_flow"]],
    ["agentkit-sandbox-guest", ["--test", "fsops_hostile"]],
    realVM,
  ]],
  ["T14", "export manifests, no host overwrite, stale grants denied", [
    ["agentkit-sandbox-worker", ["--test", "staging_flow"]],
    realVM,
  ]],
  ["T15", "persistent re-boot from private disk; next generation", [
    ["agentkit-sandbox-worker", ["--test", "lifecycle"]],
    realVM,
  ]],
  ["T16", "writer lease arbitration; no escalation to children", [
    ["agentkit-sandbox-worker", ["--test", "writer_lease"]],
    ["agentkit-sandbox-host", ["--test", "subagents_worker"]],
    realVM,
  ]],
  ["T17", "legacy host tools intact; VM-bound sessions never touch host I/O", [
    ["agentkit-toolpacks-coding", ["--test", "vm_routing"]],
    sandboxEscape,
    realVM,
  ]],
  ["T18", "protocol: version/frame/digest/path adversarial refusal", [
    ["agentkit-sandbox-contracts", ["--test", "protocol_adversarial"]],
    ["agentkit-sandbox-contracts", ["--test", "protocol_property"]],
    ["agentkit-sandbox-worker", ["--test", "fuzz_dispatch"]],
    ["agentkit-sandbox-guest", ["--test", "handshake"]],
    realVM,
  ]],
  ["T19", "revocation while alive prevents next effect", [
    ["agentkit-sandbox-worker", ["--test", "exec_flow"]],
    ["agentkit-sandbox-host", ["--test", "subagents_worker"]],
    realVM,
  ]],
  ["T20", "deletion/quarantine: verified erase or retained quarantine", [
    ["agentkit-sandbox-worker", ["--test", "lifecycle"]],
    ["agentkit-sandbox-worker", ["--test", "journal_replay"]],
    realVM,
  ]],
  ["T21", "Linux co-located app cannot read worker disks/sockets", [
    // L2-only: requires the multi-user real-host harness; honest NOT RUN
    // unless /dev/kvm + firecracker are present and the harness runs.
    realVM,
  ]],
  ["T22", "macOS co-located app cannot drive the helper without IPC", [
    appleParityL1,
    realVZ,
  ]],
  ["T23", "bounded multi-tenant soak with restarts (24h is L4 sign-off)", [
    ["agentkit-sandbox-worker", ["--test", "soak"]],
  ]],
  ["T24", "hostile guest inputs: no panic/RCE/alloc blowup/broker abuse", [
    ["agentkit-sandbox-contracts", ["--test", "protocol_adversarial"]],
    ["agentkit-sandbox-contracts", ["--test", "protocol_property"]],
    ["agentkit-sandbox-worker", ["--test", "fuzz_dispatch"]],
    ["agentkit-sandbox-guest", ["--test", "handshake"]],
    ["agentkit-sandbox-guest", ["--test", "fsops_hostile"]],
    realVM,
  ]],
];

console.log(`sandbox evidence: building harnesses (target ${targetDir}) ...`);
const rows = [];
for (const [id, scenario, evidences] of MATRIX) {
  const runs = [];
  for (const e of evidences) {
    if (e && e.gated) {
      process.stdout.write(`  ${id}: ${e.pkg} ${e.gated} (gated) ... `);
      const r = gatedRunEntry(e, quick);
      console.log(r.status + (r.notRun.length ? `: ${r.notRun[0]}` : ""));
      runs.push(r);
      continue;
    }
    const [pkg, extra, filter = []] = e;
    process.stdout.write(`  ${id}: ${pkg} ${extra.join(" ")} ... `);
    const r = cargoTest(pkg, extra, filter);
    console.log(r.status + (r.notRun.length ? ` (${r.notRun.length} NOT RUN lines)` : ""));
    runs.push(r);
  }
  const worst = runs.some((r) => r.status.startsWith("FAIL"))
    ? "FAIL"
    : runs.every((r) => r.status === "PASS")
      ? "PASS"
      : runs.some((r) => r.status === "PASS" || r.status.startsWith("PARTIAL"))
        ? "PARTIAL"
        : runs.every((r) => r.status.startsWith("NOT RUN"))
          ? "NOT RUN"
          : "UNQUALIFIED";
  rows.push({ id, scenario, status: worst, runs });
}

const date = new Date().toISOString().slice(0, 10);
const stamp = new Date().toISOString();
mkdirSync(outDir, { recursive: true });

const jsonPath = join(outDir, `sandbox-evidence-${date}.json`);
writeFileSync(
  jsonPath,
  JSON.stringify(
    { generated_at: stamp, host: probes, rows },
    (k, v) => (k === "out" ? undefined : v),
    2,
  ) + "\n",
);

const md = [];
md.push(`# Sandbox runtime — T01–T24 evidence report, ${date}`);
md.push("");
md.push(`Generated: \`${stamp}\` by \`bun scripts/sandbox-evidence.mjs\` (statuses are derived from executed commands only; hardware rows self-report \`NOT RUN\` — S19).`);
md.push("");
md.push(`- host: \`${probes.uname}\``);
md.push(`- toolchain: \`${probes.rustc}\` / \`${probes.cargo}\``);
md.push(`- /dev/kvm: ${probes.kvm}`);
md.push(`- firecracker: \`${probes.firecracker}\`; jailer: \`${probes.jailer}\``);
md.push(`- macOS VZ helper: ${probes.vzHelper}`);
md.push("");
md.push("| ID | Scenario | Status | Evidence runs |");
md.push("|----|----------|--------|---------------|");
for (const row of rows) {
  const runs = row.runs
    .map((r) => `\`${r.command}\` → ${r.status}${r.notRun.length ? `: ${r.notRun[0]}` : ""}`)
    .join("<br>");
  md.push(`| ${row.id} | ${row.scenario} | ${row.status} | ${runs} |`);
}
md.push("");
md.push("**Truthfulness policy.** L0/L1 statuses come solely from `cargo test` output of the listed commands. L2/L3 harnesses (`real_vm`, VZ parity) gate on real hardware and record `NOT RUN <reason>` themselves; an `L4` claim (24h soak, red-team sign-off) additionally requires an operator run of `AGENTKIT_SOAK_SCALE=<n> cargo test -p agentkit-sandbox-worker --test soak` and reviewer approval — never inferred from this report. Machine-readable twin: `sandbox-evidence-${date}.json`.");
md.push("");
const mdPath = join(outDir, `sandbox-evidence-${date}.md`);
writeFileSync(mdPath, md.join("\n"));
console.log(`wrote ${mdPath}`);
console.log(`wrote ${jsonPath}`);
const fails = rows.filter((r) => r.status === "FAIL");
if (fails.length) {
  console.error(`FAILING ROWS: ${fails.map((f) => f.id).join(", ")}`);
  process.exit(1);
}
