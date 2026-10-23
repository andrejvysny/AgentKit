import { describe, expect, it } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { runCrashScenario } from "./rust-runner-conformance-crash.js";
import { runRunnerScenario } from "./rust-runner-conformance-driver.js";
import {
  buildRunnerVectors,
  type Json,
  type RunnerVectors,
} from "./rust-runner-conformance-model.js";

const root = resolve(import.meta.dir, "../..");
const vectors = JSON.parse(
  await readFile(
    join(root, "rust/fixtures/conformance/runner-faults-v1.json"),
    "utf8",
  ),
) as RunnerVectors;

describe("actual TypeScript/Rust executing-runner fault protocol", () => {
  it("freezes reviewed observations and explicit safety deltas", () => {
    expect(vectors).toEqual(buildRunnerVectors());
    expect(vectors.scenarios).toHaveLength(6);
    expect(vectors.scenarios.filter((entry) => entry.delta)).toHaveLength(2);
  });
  for (const adapter of ["memory", "sqlite"] as const) {
    for (const scenario of vectors.scenarios.filter(
      (entry) => entry.name !== "crash_recovery",
    )) {
      it(`${adapter}/${scenario.name}: actual TS runner matches reviewed trace`, async () => {
        expect(await runRunnerScenario(adapter, scenario.name)).toEqual(
          scenario.expected.typescript,
        );
      }, 20_000);
    }
  }
  it("SIGKILL/reopen/repeated recovery/resume uses the actual TS file runner", async () => {
    const directory = await mkdtemp(join(tmpdir(), "agentkit-runner-ts-"));
    try {
      const expected = vectors.scenarios.find(
        (entry) => entry.name === "crash_recovery",
      )!;
      expect(await runCrashScenario(directory)).toEqual(
        expected.expected.typescript,
      );
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  }, 30_000);

  const differential =
    process.env.AGENTKIT_RUN_RUST_RUNNER_CONFORMANCE === "1" ? it : it.skip;
  differential(
    "compares both actual runtimes with explicit approved delta traces",
    async () => {
      const directory = await mkdtemp(join(tmpdir(), "agentkit-runner-diff-"));
      try {
        const report = join(directory, "native.json");
        const proc = Bun.spawn(
          [
            "cargo",
            "test",
            "--manifest-path",
            "rust/Cargo.toml",
            "-p",
            "agentkit-host",
            "--test",
            "runner_conformance",
            "--locked",
            "--offline",
            "--",
            "--nocapture",
          ],
          {
            cwd: root,
            env: { ...process.env, AGENTKIT_RUNNER_CONFORMANCE_REPORT: report },
            stdout: "pipe",
            stderr: "pipe",
          },
        );
        const [code, stdout, stderr] = await Promise.all([
          proc.exited,
          new Response(proc.stdout).text(),
          new Response(proc.stderr).text(),
        ]);
        expect(code, `${stdout}\n${stderr}`).toBe(0);
        const native = JSON.parse(await readFile(report, "utf8")) as Record<
          string,
          Record<string, Json[]>
        >;
        for (const scenario of vectors.scenarios) {
          const ts =
            scenario.name === "crash_recovery"
              ? await runCrashScenario(directory)
              : await runRunnerScenario("sqlite", scenario.name);
          expect(ts, `typescript/${scenario.name}`).toEqual(
            scenario.expected.typescript,
          );
          for (const adapter of scenario.name === "crash_recovery"
            ? ["file"]
            : ["memory", "file"]) {
            const rust = native[adapter]?.[scenario.name];
            expect(rust, `rust/${adapter}/${scenario.name}`).toEqual(
              scenario.expected.rust,
            );
            if (!scenario.delta) expect(rust).toEqual(ts);
            else expect(rust).not.toEqual(ts);
          }
        }
      } finally {
        await rm(directory, { recursive: true, force: true });
      }
    },
    120_000,
  );
});
