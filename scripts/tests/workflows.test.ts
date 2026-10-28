import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";

interface Step {
  name?: string;
  run?: string;
  uses?: string;
  env?: Record<string, string>;
  "working-directory"?: string;
}
interface Job {
  needs?: string[] | string;
  if?: string;
  "runs-on"?: string;
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
  it("keeps the packaged-consumer gate as a real step in the native matrix", () => {
    expect(commands(rust.jobs.native)).toContain(
      "node rust/scripts/check-packages.mjs --offline",
    );
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

  it("qualifies Windows as pure-native with the Bun interop suite explicitly opted out", () => {
    for (const step of rust.jobs["native-windows"]?.steps ?? [])
      expect(step.uses ?? "").not.toContain("setup-bun");
    const windowsTest = rust.jobs["native-windows"]?.steps.find((step) =>
      step.run?.includes("cargo test"),
    );
    expect(windowsTest?.env?.AGENTKIT_SKIP_TS_INTEROP).toBe("1");
    // The Unix-only coding-pack scope must stay explicit.
    expect(windowsTest?.run).toContain("--exclude agentkit-toolpacks-coding");
    expect(windowsTest?.run).toContain("--exclude agentkit-rust");
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
