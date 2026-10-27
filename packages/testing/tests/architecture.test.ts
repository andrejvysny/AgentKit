/**
 * Architecture invariants for the unified agent runtime (ADR 0015 §1, §5).
 *
 * AgentKit has exactly ONE model/tool loop. These tests pin that by source:
 * every place that calls a provider (`.streamChat(`), invokes a tool or
 * executor (`.execute(`), or drives the loop (`runChat(` / `runAgentLoop(`)
 * is listed below WITH the reason it is allowed. A new call site fails here
 * on purpose — not because it is necessarily wrong, but because a second loop
 * or a tool invocation that bypasses `executeToolSafely` is exactly the drift
 * this runtime must not acquire silently. Review it, then add it with its
 * reason.
 *
 * The scan is textual over `packages/<pkg>/src`, with comments stripped, so
 * a doc comment that mentions `tool.execute(` does not count.
 */
import { describe, expect, it } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const REPO = join(import.meta.dir, "..", "..", "..");
const PACKAGES = join(REPO, "packages");

/** Allowed `.streamChat(` call sites → why. */
const STREAM_CHAT_SITES: Record<string, string> = {
  "core/src/runs/model-step.ts": "the loop's single provider request",
  "core/src/runs/bounded-provider.ts":
    "budget wrapper delegating to the wrapped client",
};

/** Allowed `.execute(` call sites → why. */
const EXECUTE_SITES: Record<string, string> = {
  "core/src/runs/tool-exec.ts":
    "executeToolSafely: the loop's single tool invocation (2 branches)",
  "core/src/runs/bounded-run.ts":
    "budget wrapper delegating to the wrapped tool",
  "host/src/turn/registry-staging.ts":
    "ToolGuard wrapper delegating to the guarded tool",
  "host/src/tasks/executor-registry.ts":
    "task dispatch to a TaskExecutor (not a tool)",
  "runner-local/src/single-process-task-runner.ts":
    "runner dispatch to a TaskWorker (not a tool)",
  "mcp-server/src/tool-source.ts":
    "inbound MCP serving surface (no model loop)",
  "mcp-server/src/server.ts":
    "MCP tools/call routed to the tool source (no model loop)",
};

/** Allowed `runChat(` / `runAgentLoop(` call sites outside their definitions. */
const LOOP_DRIVER_SITES: Record<string, string> = {
  "core/src/runs/run-loop.ts": "runChat facade over runAgentLoop",
  "host/src/turn/turn-runner.ts": "TurnRunner.runPass, the one host driver",
  "host/src/context/compactor.ts":
    "session context compaction: one tool-less summarization request",
};

const FORBIDDEN_DEPENDENCY = /^@(earendil-works|mariozechner)\//;

function sourceFiles(dir: string): string[] {
  const files: string[] = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) files.push(...sourceFiles(path));
    else if (/\.(ts|tsx|mts|mjs|js)$/.test(name)) files.push(path);
  }
  return files;
}

/** Every `packages/<pkg>/src` file, keyed `<pkg>/src/...`. */
function packageSources(): Map<string, string> {
  const sources = new Map<string, string>();
  for (const pkg of readdirSync(PACKAGES)) {
    const src = join(PACKAGES, pkg, "src");
    try {
      if (!statSync(src).isDirectory()) continue;
    } catch {
      continue;
    }
    for (const file of sourceFiles(src))
      sources.set(relative(PACKAGES, file), readFileSync(file, "utf8"));
  }
  return sources;
}

/**
 * Crude comment stripper: block comments, then line comments that start a
 * line or follow whitespace. Good enough for call-site counting — it can
 * over-strip inside a string containing `//`, which only ever hides a match.
 */
function stripComments(source: string): string {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|\s)\/\/.*$/gm, "$1");
}

function sitesMatching(pattern: RegExp): Map<string, number> {
  const sites = new Map<string, number>();
  for (const [path, source] of packageSources()) {
    const count = stripComments(source).match(pattern)?.length ?? 0;
    if (count > 0) sites.set(path, count);
  }
  return sites;
}

describe("architecture: one agent loop (ADR 0015)", () => {
  it("scans a non-trivial source tree", () => {
    expect(packageSources().size).toBeGreaterThan(100);
  });

  it("calls a provider only from the loop's model step", () => {
    const sites = sitesMatching(/\.streamChat\(/g);
    expect([...sites.keys()].sort()).toEqual(
      Object.keys(STREAM_CHAT_SITES).sort(),
    );
    expect(sites.get("core/src/runs/model-step.ts")).toBe(1);
  });

  it("invokes tools and executors only from the allowlisted sites", () => {
    const sites = sitesMatching(/\.execute\(/g);
    expect([...sites.keys()].sort()).toEqual(Object.keys(EXECUTE_SITES).sort());
  });

  it("drives the loop only from the runChat facade and TurnRunner", () => {
    const sites = sitesMatching(
      /(?<!function\s)(?<![\w.])(runChat|runAgentLoop)\(/g,
    );
    expect([...sites.keys()].sort()).toEqual(
      Object.keys(LOOP_DRIVER_SITES).sort(),
    );
  });
});

describe("architecture: Pi is a reference, not a runtime dependency", () => {
  it("no package.json depends on a Pi package", () => {
    const manifests = [
      join(REPO, "package.json"),
      ...readdirSync(PACKAGES).map((pkg) =>
        join(PACKAGES, pkg, "package.json"),
      ),
    ];
    const offenders: string[] = [];
    for (const manifest of manifests) {
      let json: Record<string, unknown>;
      try {
        json = JSON.parse(readFileSync(manifest, "utf8"));
      } catch {
        continue;
      }
      for (const field of [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
      ]) {
        const deps = json[field];
        if (deps === null || typeof deps !== "object") continue;
        for (const name of Object.keys(deps))
          if (FORBIDDEN_DEPENDENCY.test(name))
            offenders.push(`${relative(REPO, manifest)}: ${name}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it("no source imports a Pi package", () => {
    const offenders = [...packageSources()]
      .filter(([, source]) =>
        /from\s+["']@(earendil-works|mariozechner)\//.test(source),
      )
      .map(([path]) => path);
    expect(offenders).toEqual([]);
  });
});
