import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  foundationExclusions,
  projectFoundation,
  sha256,
} from "../release/foundation.mjs";

import { sourceFixture } from "./snapshot-fixture";

const root = fileURLToPath(new URL("../../", import.meta.url));
const integrated = existsSync(
  join(root, "packages/core/src/providers/responses.ts"),
);
let scratch: string;
let fixtureRoot: string;

function snapshot(name: string, release: string): string {
  const directory = join(scratch, name);
  const version = release === "foundation" ? "0.6.0" : "0.7.0";
  const result = spawnSync(
    "node",
    ["scripts/snapshot-release.mjs", directory, version, release],
    {
      cwd: fixtureRoot,
      encoding: "utf8",
    },
  );
  if (result.status !== 0) throw new Error(result.stderr);
  return directory;
}

function read(directory: string, path: string): string {
  return readFileSync(join(directory, path), "utf8");
}

describe.skipIf(!integrated)("current-source foundation projection", () => {
  beforeAll(() => {
    scratch = mkdtempSync(join(tmpdir(), "agentkit-snapshot-test-"));
    fixtureRoot = sourceFixture(root, join(scratch, "source"));
  });
  afterAll(() => {
    rmSync(scratch, { recursive: true, force: true });
  });

  it("reproduces independently and preserves current generic source", () => {
    const first = snapshot("foundation-a", "foundation");
    const second = snapshot("foundation-b", "foundation");
    expect(read(first, "snapshot-provenance.json")).toBe(
      read(second, "snapshot-provenance.json"),
    );
    for (const path of foundationExclusions)
      expect(existsSync(join(first, path))).toBe(false);
    for (const path of [
      "packages/core/src/providers/sse.ts",
      "packages/core/src/providers/openai-compatible.ts",
      "packages/client/src/sse.ts",
      "packages/client/src/stream.ts",
      "packages/transport-http/src/sse.ts",
      "packages/adapters-sqlite/src/sqlite/open.ts",
      "examples/desktop-host/node-host.mjs",
    ])
      expect(read(first, path)).toBe(read(fixtureRoot, path));
    expect(read(first, "packages/host/src/turn/projection.ts")).toContain(
      "if (!(key in snapshot)) delete current[key]",
    );
    expect(
      read(first, "packages/adapters-sqlite/src/sqlite/migrations.ts"),
    ).toContain("unsupportedVersion(version, target)");
    expect(read(first, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 8",
    );
    expect(read(first, "packages/agentkit/package.json")).toContain(
      '"version": "0.6.0"',
    );
    const provenance = JSON.parse(read(first, "snapshot-provenance.json"));
    expect(provenance.patches).toHaveLength(6);
    for (const patch of provenance.patches)
      expect(patch.sha256).toBe(sha256(read(first, patch.path)));
    expect(
      provenance.excluded.every((file: { sha256: string }) =>
        /^[a-f0-9]{64}$/.test(file.sha256),
      ),
    ).toBe(true);
    expect(
      provenance.originalFiles.some(
        (file: { path: string }) =>
          file.path === "packages/core/src/providers/responses.ts",
      ),
    ).toBe(true);
  });

  it("fails closed on changed integration before removing excluded files", () => {
    const directory = snapshot("drift", "responses");
    const path = "packages/core/src/providers/client.ts";
    writeFileSync(
      join(directory, path),
      read(directory, path).replace(
        "continuationRequired?: boolean",
        "continuationRequired?: string",
      ),
    );
    expect(() => projectFoundation(directory)).toThrow(
      "Foundation projection drift",
    );
    expect(
      existsSync(join(directory, "packages/core/src/providers/responses.ts")),
    ).toBe(true);
    expect(read(directory, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 9",
    );
  });

  it("applies captured patches rather than reading mutable checkout patches", () => {
    const directory = snapshot("captured-patches", "responses");
    const path = "scripts/release/foundation/sqlite.patch";
    writeFileSync(
      join(directory, path),
      read(directory, path).replace(
        "SCHEMA_VERSION = 9",
        "SCHEMA_VERSION = 999",
      ),
    );
    expect(() => projectFoundation(directory)).toThrow(
      "Foundation projection drift",
    );
  });

  it("retains Responses source and schema 9 in separate candidate", () => {
    const directory = snapshot("responses", "responses");
    expect(read(directory, "packages/core/src/providers/responses.ts")).toBe(
      read(fixtureRoot, "packages/core/src/providers/responses.ts"),
    );
    expect(read(directory, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 9",
    );
    const provenance = JSON.parse(read(directory, "snapshot-provenance.json"));
    expect(provenance.patches).toEqual([]);
    expect(provenance.excluded).toEqual([]);
  });
});
