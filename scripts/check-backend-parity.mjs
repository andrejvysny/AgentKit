#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

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
  [
    "toolpacks-coding",
    "TypeScript coding facade; not native qualification",
    56,
  ],
  ["mcp-client", "TypeScript MCP facade; not native qualification", 56],
  ["mcp-server", "TypeScript MCP facade; not native qualification", 56],
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
  const count = (kind) => {
    const summary = output.match(new RegExp(`(?:^|\\n)\\s*(\\d+) ${kind}\\b`));
    return summary
      ? Number(summary[1])
      : [...output.matchAll(new RegExp(`^\\(${kind}\\)`, "gm"))].length;
  };
  const row = {
    name,
    implementation,
    workItem: `AGENTKIT-${issue}`,
    status: result.status,
    pass: count("pass"),
    fail: count("fail"),
    skip: count("skip"),
    todo: count("todo"),
    error: result.error?.message ?? null,
    signal: result.signal,
    complete: /Ran \d+ tests? across/.test(output),
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
