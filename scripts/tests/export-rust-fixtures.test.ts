import { describe, expect, it } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { TypeCompiler } from "@sinclair/typebox/compiler";
import {
  AiRunEventSchema,
  REST_ROUTES,
} from "../../packages/contracts/src/index.js";
import {
  buildFixtures,
  canonicalJson,
  exportFixtures,
  FIXTURE_DIR,
} from "../export-rust-fixtures.js";

describe("Rust parity fixture exporter", () => {
  it("is deterministic, complete and matches the committed freeze", async () => {
    const first = await buildFixtures();
    expect([...(await buildFixtures())]).toEqual([...first]);
    expect(await exportFixtures(FIXTURE_DIR, true)).toEqual([]);
    const routes = JSON.parse(first.get("routes.json")!);
    expect(routes.routes).toHaveLength(48);
    expect(
      routes.routes.map((route: { operation: string }) => route.operation),
    ).toEqual(Object.keys(REST_ROUTES));
    const events = JSON.parse(first.get("run-events.json")!);
    expect(events.variants).toHaveLength(14);
    expect(first.size).toBe(14);
    const validator = TypeCompiler.Compile(AiRunEventSchema);
    for (const [path, body] of first) {
      if (!path.startsWith("golden-traces/")) continue;
      const trace: unknown[] = JSON.parse(body);
      expect(trace.length).toBeGreaterThan(0);
      for (const event of trace) expect(validator.Check(event)).toBe(true);
    }
  });

  it("preserves optionality, unions, open objects and array ordering", () => {
    expect(canonicalJson({ z: [3, 1], a: { y: true, x: null } })).toBe(
      '{\n  "a": {\n    "x": null,\n    "y": true\n  },\n  "z": [\n    3,\n    1\n  ]\n}\n',
    );
  });

  it("detects changed, missing and unexpected files without repairing them", async () => {
    const directory = await mkdtemp(join(tmpdir(), "agentkit-rust-fixtures-"));
    try {
      expect(await exportFixtures(directory)).toEqual([]);
      await mkdir(join(directory, "conformance"));
      await writeFile(join(directory, "conformance", "authored.json"), "{}\n");
      await writeFile(join(directory, "schema-validation.json"), "{}\n");
      expect(await exportFixtures(directory, true)).toEqual([]);
      await writeFile(join(directory, "schemas.json"), "changed\n");
      await rm(join(directory, "routes.json"));
      await writeFile(join(directory, "stale.json"), "{}\n");
      expect(await exportFixtures(directory, true)).toEqual([
        "routes.json",
        "schemas.json",
        "stale.json",
      ]);
      expect(await readFile(join(directory, "schemas.json"), "utf8")).toBe(
        "changed\n",
      );
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  });

  it("freezes operation domain separation and canonical request fingerprints", async () => {
    const files = await buildFixtures();
    const data = JSON.parse(files.get("idempotency.json")!);
    expect(data.fingerprints[0].fingerprint).toBe(
      data.fingerprints[1].fingerprint,
    );
    expect(data.fingerprints[4].fingerprint).not.toBe(
      data.fingerprints[5].fingerprint,
    );
    expect(data.vectors[0].submitTaskId).not.toBe(
      data.vectors[0].regenerateTaskId,
    );
    expect(data.vectors[0].submitTaskId).not.toBe(data.vectors[1].submitTaskId);
    expect(data.vectors[0].regenerateTaskId).not.toBe(
      data.vectors[2].regenerateTaskId,
    );
    expect(data.vectors[0].regenerateTaskId.slice(8)).toBe(
      data.vectors[3].submitTaskId.slice(8),
    );
  });
});
