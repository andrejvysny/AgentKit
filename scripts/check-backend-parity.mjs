#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parseParitySummary } from "./qualification/parity-summary.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
const directory =
  process.env.AGENTKIT_PARITY_OUTPUT ?? join(tmpdir(), "agentkit-parity");
const selected = process.argv.slice(2);
const suites = [
  ["core", "native loop", 53],
  ["host", "TypeScript host with native loop", 54],
  ["adapters-memory", "TypeScript adapter; not native qualification", 55],
  ["adapters-sqlite", "TypeScript adapter; not native qualification", 55],
  ["runner-local", "TypeScript runner with native loop", 55],
  ["mcp-client", "TypeScript MCP facade; not native qualification", 56],
  ["transport-http", "TypeScript server; not native qualification", 56],
];
for (const name of selected) {
  if (!suites.some(([known]) => known === name))
    throw new Error(`Unknown parity suite: ${name}`);
}

// A missing addon must fail before a suite can accidentally pass in TypeScript.
const binding = await import("../packages/native/generated/index.js");
if (binding.bindingSmoke() !== "agentkit-native")
  throw new Error("Invalid native addon");
mkdirSync(directory, { recursive: true });
const results = [];
for (const [name, implementation, issue] of suites) {
  if (selected.length && !selected.includes(name)) continue;
  const result = spawnSync("bun", ["test", `packages/${name}`], {
    cwd: root,
    env: {
      ...process.env,
      AGENTKIT_BACKEND: "native",
      AGENTKIT_REQUIRE_NATIVE: "1",
    },
    encoding: "utf8",
    timeout: 180_000,
    maxBuffer: 64 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  writeFileSync(join(directory, `${name}.log`), output);
  const summary = parseParitySummary(output);
  // A lane killed by the per-suite timeout (e.g. spawnSync's 180 s cap
  // under a loaded host) produces truncated output: its counts stay
  // whatever the markers show, and `complete:false` plus `killed` tell the
  // reader this lane proved nothing — never a pass, and never fabricated
  // numbers either.
  const row = {
    name,
    implementation,
    workItem: `AGENTKIT-${issue}`,
    status: result.status,
    pass: summary.pass,
    fail: summary.fail,
    skip: summary.skip,
    todo: summary.todo,
    error: result.error?.message ?? null,
    signal: result.signal,
    complete: summary.complete,
    killed: result.signal != null || result.error != null,
  };
  results.push(row);
  console.log(JSON.stringify(row));
}
writeFileSync(
  join(directory, "results.json"),
  `${JSON.stringify(
    {
      commit: spawnSync("git", ["rev-parse", "HEAD"], {
        cwd: root,
        encoding: "utf8",
      }).stdout.trim(),
      workingTree:
        "Includes uncommitted migration changes; not release qualification",
      platform: `${process.platform}-${process.arch}`,
      node: process.version,
      bun: spawnSync("bun", ["--version"], { encoding: "utf8" }).stdout.trim(),
      results,
    },
    null,
    2,
  )}\n`,
);
process.exitCode = results.some((row) => row.status !== 0 || row.error !== null)
  ? 1
  : 0;
