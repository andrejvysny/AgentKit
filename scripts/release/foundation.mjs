import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";

export const foundationPatches = [
  "contracts.patch",
  "core.patch",
  "host.patch",
  "memory.patch",
  "sqlite.patch",
  "docs.patch",
];
export const foundationExclusions = [
  "docs/providers/responses.md",
  "packages/contracts/src/provider-continuation.ts",
  "packages/core/src/providers/responses.ts",
  "packages/core/src/providers/responses-types.ts",
  "packages/core/src/providers/responses-tools.ts",
  "packages/core/src/providers/responses-request.ts",
  "packages/core/src/providers/responses-stream.ts",
  "packages/core/tests/responses.test.ts",
  "packages/core/tests/responses-request.test.ts",
  "packages/core/tests/responses-continuation.test.ts",
  "packages/core/tests/responses-accounting.test.ts",
  "packages/core/tests/responses-helpers.ts",
  "packages/core/tests/run-loop-continuation.test.ts",
  "packages/adapters-memory/src/memory-provider-continuation-store.ts",
  "packages/adapters-sqlite/src/provider-continuation-schema.ts",
  "packages/adapters-sqlite/src/sqlite/provider-continuation-store.ts",
  "packages/adapters-sqlite/tests/provider-continuation-durability.test.ts",
  "packages/adapters-sqlite/tests/provider-continuation.test.ts",
  "packages/host/src/continuations/validation.ts",
  "packages/host/src/ports/provider-continuation-store.ts",
  "packages/host/src/turn/canonical-projection.ts",
  "packages/host/src/turn/provider-continuation.ts",
  "packages/host/tests/provider-continuation-cancellation.test.ts",
  "packages/host/tests/provider-continuation-durability.test.ts",
  "packages/host/tests/provider-continuation-helpers.ts",
  "packages/host/tests/provider-continuation-projection.test.ts",
  "packages/host/tests/provider-continuation.test.ts",
  "packages/host/tests/responses-openpcb.test.ts",
  "packages/host/tests/responses-openpcb-helpers.ts",
  "packages/testing/fixtures/openpcb/catalog.json",
  "packages/testing/fixtures/openpcb/README.md",
  "scripts/qualification/responses-openpcb-smoke.mjs",
];

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function apply(directory, paths, check) {
  const result = spawnSync(
    "git",
    ["apply", ...(check ? ["--check"] : []), ...paths],
    { cwd: directory, encoding: "utf8" },
  );
  if (result.status !== 0)
    throw new Error(`Foundation projection drift: ${result.stderr.trim()}`);
}

export function projectFoundation(directory) {
  const paths = foundationPatches.map((name) =>
    join(directory, "scripts/release/foundation", name),
  );
  const patches = paths.map((path, index) => ({
    path: `scripts/release/foundation/${foundationPatches[index]}`,
    sha256: sha256(readFileSync(path)),
  }));
  // Validate all hunks before changing any source; never fall back to old files.
  apply(directory, paths, true);
  apply(directory, paths, false);
  for (const path of foundationExclusions)
    rmSync(join(directory, path), { force: true });
  assertFoundation(directory);
  return patches;
}

export function assertFoundation(directory) {
  for (const path of foundationExclusions)
    if (existsSync(join(directory, path)))
      throw new Error(`Foundation retained excluded source: ${path}`);
  const schema = readFileSync(
    join(directory, "packages/adapters-sqlite/src/schema.ts"),
    "utf8",
  );
  if (!schema.includes("export const SCHEMA_VERSION = 8;"))
    throw new Error("Foundation must use SQLite schema 8");
}
