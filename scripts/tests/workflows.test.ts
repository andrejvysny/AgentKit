import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";

interface Step {
  name?: string;
  run?: string;
  uses?: string;
  if?: string;
  env?: Record<string, string>;
  "working-directory"?: string;
}
interface Job {
  needs?: string[] | string;
  if?: string;
  "runs-on"?: string;
  strategy?: { matrix?: Record<string, Array<string | number>> };
  steps?: Step[];
}
interface Workflow {
  jobs: Record<string, Job>;
}

const workflowFiles = [
  "../../.github/workflows/ci.yml",
  "../../.github/workflows/nightly.yml",
  "../../.github/workflows/publish.yml",
  "../../.github/workflows/release.yml",
  "../../.github/workflows/rust.yml",
];

function load(path: string): Promise<Workflow> {
  return Bun.file(new URL(path, import.meta.url))
    .text()
    .then((text) => Bun.YAML.parse(text) as Workflow);
}

const rust = await load("../../.github/workflows/rust.yml");

function commands(job: Job | undefined): string[] {
  return (job?.steps ?? []).flatMap((step) => (step.run ? [step.run] : []));
}

function gate(results: Record<string, { result: string }>): number | null {
  const script = rust.jobs.qualification?.steps[0]?.run;
  if (!script) throw new Error("Missing native qualification gate script");
  return spawnSync("bash", ["-e", "-c", script], {
    env: { ...process.env, RESULTS: JSON.stringify(results) },
    encoding: "utf8",
  }).status;
}

describe("workflow structural validity", () => {
  it("every step in every workflow executes something", async () => {
    for (const file of workflowFiles) {
      const workflow = await load(file);
      for (const [jobName, job] of Object.entries(workflow.jobs)) {
        expect(job.steps, `${file} job ${jobName}`).toBeDefined();
        for (const [index, step] of (job.steps ?? []).entries()) {
          const has = [Boolean(step.run), Boolean(step.uses)];
          // A name-only step (or a step with both or neither) is the defect
          // that made the whole Rust workflow fail to schedule: the runner
          // rejects it before any job starts.
          expect(
            has.filter(Boolean).length,
            `${file} ${jobName} step ${index} (${step.name ?? "unnamed"}) must define exactly one of run/uses`,
          ).toBe(1);
          if (step.run !== undefined)
            expect(
              step.run.trim().length,
              `${file} ${jobName} step ${index}`,
            ).toBeGreaterThan(0);
          if (step.uses !== undefined)
            expect(
              step.uses.trim().length,
              `${file} ${jobName} step ${index}`,
            ).toBeGreaterThan(0);
          if (step.name !== undefined) expect(typeof step.name).toBe("string");
        }
      }
    }
  });
});

describe("Rust native workflow qualification", () => {
  it("requires native execution and installed artifacts on Node 22 and 24", () => {
    for (const name of ["native", "native-windows"]) {
      const job = rust.jobs[name];
      expect(job.strategy?.matrix?.["node-version"]).toEqual([22, 24]);
      const setup = job.steps?.find((step) =>
        step.uses?.includes("setup-node"),
      );
      expect(setup).toBeDefined();
      const core = job.steps?.find(
        (step) => step.run === "bun test packages/core",
      );
      expect(core?.env?.AGENTKIT_BACKEND).toBe("native");
      expect(core?.env?.AGENTKIT_REQUIRE_NATIVE).toBe("1");
      expect(core?.if).toBeUndefined();
      for (const script of [
        "check-native-package.mjs",
        "check-native-umbrella.mjs",
      ]) {
        const gate = job.steps?.find((step) => step.run?.includes(script));
        expect(gate, `${name} ${script}`).toBeDefined();
        expect(gate?.if).toBeUndefined();
      }
    }
  });

  it("keeps the packaged-consumer gates as real steps in the native matrix", () => {
    expect(commands(rust.jobs.native)).toContain(
      "node rust/scripts/check-packages.mjs --offline",
    );
    // The packed @agentkit/native install gate must also be a required
    // step — a tarball a consumer cannot install must not ship silently.
    expect(commands(rust.jobs.native)).toContain(
      "node rust/scripts/check-native-package.mjs",
    );
    // And it runs on EVERY matrix OS: the packed acceptance lifecycle
    // (approval round-trip, cancel, awaited close, fresh-process reopen)
    // qualifies the artifact on the platform that built it — an
    // `if: runner.os` restriction here would silently narrow the gate.
    const packedStep = (rust.jobs.native?.steps ?? []).find((step) =>
      (step.run ?? "").includes("check-native-package.mjs"),
    );
    expect(packedStep).toBeDefined();
    expect(packedStep.if).toBeUndefined();
  });

  it("runs the mixed Bun interop suite as a required step without the skip opt-out", () => {
    const interop = commands(rust.jobs.native).find((command) =>
      command.includes("--test typescript_boundaries"),
    );
    expect(interop).toBeDefined();
    for (const needle of ["--test interoperability", "--test all_routes"])
      expect(interop).toContain(needle);
    for (const step of rust.jobs.native?.steps ?? [])
      expect(step.env?.AGENTKIT_SKIP_TS_INTEROP).toBeUndefined();
  });

  it("qualifies Windows self-contained: Bun-provisioned MCP interop is required, pure-native scope stays explicit", () => {
    const windows = rust.jobs["native-windows"];
    const uses = (windows?.steps ?? []).map((step) => step.uses ?? "");
    // Bun is provisioned so the mixed-runtime MCP fixture tests genuinely
    // run on Windows — the job must not be a vacuous skip-only scope.
    expect(uses.some((use) => use.includes("setup-bun"))).toBe(true);
    expect(commands(windows)).toContain(
      "bun install --frozen-lockfile --ignore-scripts",
    );
    const windowsTest = windows?.steps.find((step) =>
      step.run?.includes("cargo test --workspace"),
    );
    // The workspace-wide pure-native step still opts the client interop
    // suite (native job's mixed-runtime gate) out explicitly, and the
    // Unix-only coding-pack exclusion stays named.
    expect(windowsTest?.env?.AGENTKIT_SKIP_TS_INTEROP).toBe("1");
    expect(windowsTest?.run).toContain("--exclude agentkit-toolpacks-coding");
    expect(windowsTest?.run).toContain("--exclude agentkit-rust");
    // The Bun-spawning MCP fixture tests are a REQUIRED Windows step: no
    // skip opt-out, so a missing Bun or fixture SDK fails loudly instead of
    // passing vacuously.
    const mcpInterop = windows?.steps.find((step) =>
      step.run?.includes("-p agentkit-mcp"),
    );
    expect(mcpInterop).toBeDefined();
    for (const needle of [
      "--test bridge",
      "--test native",
      "--test security",
      "--test shapes",
      "--test manager",
    ])
      expect(mcpInterop?.run).toContain(needle);
    expect(mcpInterop?.env?.AGENTKIT_SKIP_TS_INTEROP).toBeUndefined();
  });

  it("aggregates native qualification so skipped or missing jobs never pass", () => {
    const required = ["contracts", "native", "native-windows"];
    expect(rust.jobs.qualification?.if).toBe("always()");
    expect([...(rust.jobs.qualification?.needs ?? [])].sort()).toEqual(
      required,
    );
    const results = Object.fromEntries(
      required.map((name) => [name, { result: "success" }]),
    );
    expect(gate(results)).toBe(0);
    for (const name of required)
      for (const result of ["failure", "cancelled", "skipped"])
        expect(gate({ ...results, [name]: { result } })).toBe(1);
    const missing = { ...results };
    delete (missing as Record<string, never>)[required[0]];
    expect(gate(missing)).toBe(1);
  });
});
