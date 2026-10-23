import { describe, expect, it } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { clock, runScenario } from "./rust-store-conformance-driver.js";
import {
  buildVectors,
  type Json,
  type Vectors,
} from "./rust-store-conformance-model.js";

const root = resolve(import.meta.dir, "../..");
const vectors = JSON.parse(
  await readFile(
    join(root, "rust/fixtures/conformance/seeded-v1.json"),
    "utf8",
  ),
) as Vectors;

describe("shared Rust/TypeScript seeded store and recovery protocol", () => {
  it("freezes reproducible seeds and operation/checkpoint expectations", () => {
    expect(vectors).toEqual(buildVectors());
    expect(vectors.scenarios).toHaveLength(4);
    expect(
      vectors.scenarios.every((scenario) => scenario.steps.length >= 50),
    ).toBe(true);
  });

  for (const adapter of ["memory", "sqlite"] as const) {
    it(`${adapter}: every seeded operation and fault matches the shared oracle`, async () => {
      for (const scenario of vectors.scenarios) {
        const time = clock();
        const store =
          adapter === "memory"
            ? new MemoryAssistantStore({ clock: time, leaseTtlMs: 100 })
            : new SqliteAssistantStore(":memory:", {
                clock: time,
                leaseTtlMs: 100,
              });
        try {
          const transcript = await runScenario(store, time, scenario);
          for (const [index, step] of scenario.steps.entries()) {
            expect(
              transcript[index],
              `${adapter}/${scenario.name}/${index}/${step.action.op}`,
            ).toEqual(step.expected);
          }
        } finally {
          if (store instanceof SqliteAssistantStore) store.close();
        }
      }
    });
  }

  const differential =
    process.env.AGENTKIT_RUN_RUST_CONFORMANCE === "1" ? it : it.skip;
  differential(
    "compares native file/memory transcripts with both TypeScript adapters",
    async () => {
      const directory = await mkdtemp(join(tmpdir(), "agentkit-conformance-"));
      try {
        const report = join(directory, "rust.json");
        const proc = Bun.spawn(
          [
            "cargo",
            "test",
            "--manifest-path",
            "rust/Cargo.toml",
            "-p",
            "agentkit-store",
            "--test",
            "conformance_vectors",
            "--locked",
            "--offline",
            "--",
            "--nocapture",
          ],
          {
            cwd: root,
            env: { ...process.env, AGENTKIT_CONFORMANCE_REPORT: report },
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
          const time = clock();
          const memory = new MemoryAssistantStore({
            clock: time,
            leaseTtlMs: 100,
          });
          const expected = await runScenario(memory, time, scenario);
          expect(native.memory?.[scenario.name]).toEqual(expected);
          expect(native.file?.[scenario.name]).toEqual(expected);
          const sqliteTime = clock();
          const sqlite = new SqliteAssistantStore(":memory:", {
            clock: sqliteTime,
            leaseTtlMs: 100,
          });
          try {
            const actual = await runScenario(sqlite, sqliteTime, scenario);
            expect(native.memory?.[scenario.name]).toEqual(actual);
            expect(native.file?.[scenario.name]).toEqual(actual);
          } finally {
            sqlite.close();
          }
        }
      } finally {
        await rm(directory, { recursive: true, force: true });
      }
    },
    120_000,
  );
});
