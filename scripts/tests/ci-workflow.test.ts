import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";

interface Step {
  run?: string;
  env?: Record<string, string>;
  uses?: string;
}
interface Job {
  needs?: string[];
  if?: string;
  strategy?: {
    "fail-fast": boolean;
    matrix: Record<string, Array<string | number>>;
  };
  steps: Step[];
  "continue-on-error"?: boolean;
}
interface Workflow {
  on: Record<string, unknown>;
  jobs: Record<string, Job>;
}
const workflow = Bun.YAML.parse(
  await Bun.file(
    new URL("../../.github/workflows/ci.yml", import.meta.url),
  ).text(),
) as Workflow;
const jobs = workflow.jobs;
const required = [
  "bun",
  "lint",
  "node-smoke",
  "pack-smoke",
  "umbrella-smoke",
  "sqlite-node",
  "sqlite-bun",
];

function commands(name: string): string[] {
  return (
    jobs[name]?.steps.flatMap((step) => (step.run ? [step.run] : [])) ?? []
  );
}

function gate(results: Record<string, { result: string }>): number | null {
  const script = jobs.qualification?.steps[0]?.run;
  if (!script) throw new Error("Missing qualification gate script");
  return spawnSync("bash", ["-e", "-c", script], {
    env: { ...process.env, RESULTS: JSON.stringify(results) },
    encoding: "utf8",
  }).status;
}

describe("routine CI runtime qualification", () => {
  it("runs independent required checks on both pushes and pull requests", () => {
    expect(Object.keys(workflow.on).sort()).toEqual(["pull_request", "push"]);
    for (const name of required) {
      expect(jobs[name]).toBeDefined();
      expect(jobs[name]?.needs).toBeUndefined();
      expect(jobs[name]?.["continue-on-error"]).toBeUndefined();
      expect(commands(name)).toContain("bun install --frozen-lockfile");
      if (name !== "lint") expect(commands(name)).toContain("bun run build");
    }
  });

  it("keeps all Bun versions and portable Node support visible after failures", () => {
    for (const name of ["bun", "sqlite-bun"]) {
      expect(jobs[name]?.strategy?.["fail-fast"]).toBe(false);
      expect(jobs[name]?.strategy?.matrix["bun-version"]).toEqual([
        "1.3.14",
        "latest",
      ]);
    }
    expect(jobs["node-smoke"]?.strategy?.matrix["node-version"]).toEqual([
      20, 22, 24,
    ]);
    expect(jobs["sqlite-node"]?.strategy?.matrix["node-version"]).toEqual([
      22, 24,
    ]);
    for (const name of ["node-smoke", "sqlite-node"])
      expect(jobs[name]?.strategy?.["fail-fast"]).toBe(false);
  });

  it("executes conformance and durability against freshly built SQLite dists", () => {
    expect(commands("sqlite-node")).toContain(
      "node --test scripts/node-sqlite-conformance.mjs scripts/sqlite-durability.mjs",
    );
    for (const name of ["sqlite-node", "sqlite-bun"])
      expect(commands(name)).toContain("bun run build:umbrella");
    expect(
      jobs["sqlite-bun"]?.steps.find((step) =>
        step.run?.includes("scripts/sqlite-durability.mjs"),
      )?.env,
    ).toEqual({ AGENTKIT_SQLITE_DRIVER: "bun" });
    expect(commands("sqlite-bun")).toContain(
      "bun test ./scripts/sqlite-durability.mjs",
    );
  });

  it("keeps import, package installation, and exact artifact checks", () => {
    expect(commands("node-smoke")).toContain("node scripts/node-smoke.mjs");
    expect(commands("pack-smoke")).toContain("node scripts/pack-smoke.mjs");
    expect(commands("umbrella-smoke")).toContain("bun run smoke:umbrella");
    const qualification = commands("umbrella-smoke").find((command) =>
      command.includes("scripts/qualify-package.mjs"),
    );
    expect(qualification).toContain("scripts/source-digest.mjs");
    expect(qualification).toContain("npm pack ./packages/agentkit");
    expect(qualification).toContain('--tarball "$RUNNER_TEMP/$TARBALL"');
    expect(qualification).toContain('--source-digest "$SOURCE_DIGEST"');
  });

  it("accepts only success for every required job, including matrix results", () => {
    expect(jobs.qualification?.if).toBe("always()");
    expect(jobs.qualification?.needs).toEqual(required);
    const results = Object.fromEntries(
      required.map((name) => [name, { result: "success" }]),
    );
    expect(gate(results)).toBe(0);
    for (const name of required) {
      for (const result of ["failure", "cancelled", "skipped"])
        expect(gate({ ...results, [name]: { result } })).toBe(1);
      const missing = { ...results };
      delete missing[name];
      expect(gate(missing)).toBe(1);
    }
  });
});
