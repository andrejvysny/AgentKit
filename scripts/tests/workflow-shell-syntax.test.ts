import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// Substring assertions over workflow text cannot catch a shell block that
// fails to PARSE (a `{ …; fi` guard broke every native-release dispatch).
// Every bash-run step is fed to `bash -n`; Windows steps default to pwsh
// and are only checked when they opt into `shell: bash`.
const workflows = join(import.meta.dir, "../../.github/workflows");

type Step = { name?: string; run?: string; shell?: string };
type Job = { "runs-on"?: unknown; steps?: Step[]; defaults?: unknown };

function bashSteps(): { label: string; script: string }[] {
  const out: { label: string; script: string }[] = [];
  for (const file of readdirSync(workflows).filter((f) => /\.ya?ml$/.test(f))) {
    const doc = Bun.YAML.parse(readFileSync(join(workflows, file), "utf8")) as {
      jobs?: Record<string, Job>;
    };
    for (const [jobName, job] of Object.entries(doc.jobs ?? {})) {
      const windows = JSON.stringify(job["runs-on"] ?? "").includes("windows");
      for (const [index, step] of (job.steps ?? []).entries()) {
        if (typeof step.run !== "string") continue;
        const shell = step.shell ?? (windows ? "pwsh" : "bash");
        if (!shell.startsWith("bash")) continue;
        out.push({
          label: `${file} › ${jobName} › ${step.name ?? `step ${index}`}`,
          script: step.run,
        });
      }
    }
  }
  return out;
}

describe("workflow shell blocks", () => {
  const steps = bashSteps();

  it("finds the bash steps to check", () => {
    expect(steps.length).toBeGreaterThan(20);
  });

  it("every bash run block parses", () => {
    const broken = steps.flatMap(({ label, script }) => {
      const result = spawnSync("bash", ["-n"], {
        input: script,
        encoding: "utf8",
      });
      return result.status === 0 ? [] : [`${label}: ${result.stderr.trim()}`];
    });
    expect(broken).toEqual([]);
  });
});
