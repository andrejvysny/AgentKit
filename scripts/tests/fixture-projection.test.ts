import { afterEach, expect, test } from "bun:test";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { projectCandidateFixtures } from "../release/fixture-projection.mjs";
import { sha256 } from "../release/foundation.mjs";

const roots = ["rust/fixtures", "rust/crates/contracts/tests/fixtures"];
const directories: string[] = [];
const read = (root: string, path: string): string =>
  readFileSync(join(root, path), "utf8");
function write(root: string, path: string, body: string): void {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), body);
}
function fixture(): string {
  const root = mkdtempSync(join(tmpdir(), "agentkit-fixture-projection-"));
  directories.push(root);
  const schemas = {
    AiProviderContinuationSchema: { type: "object" },
    ExampleSchema: {
      properties: { contractVersion: { const: "0.7.0", type: "string" } },
    },
    VersionDtoSchema: { properties: { contractVersion: { type: "string" } } },
  };
  const bodies = new Map([
    ["schemas.json", `${JSON.stringify(schemas)}\n`],
    ["run-events.json", `${JSON.stringify({ contractVersion: "0.7.0" })}\n`],
  ]);
  const manifest = {
    contractVersion: "0.7.0",
    schemaCount: 3,
    sha256: Object.fromEntries(
      [...bodies].map(([path, body]) => [path, sha256(body)]),
    ),
  };
  for (const path of roots) {
    for (const [file, body] of bodies) write(root, `${path}/${file}`, body);
    write(root, `${path}/manifest.json`, JSON.stringify(manifest));
  }
  write(
    root,
    "rust/crates/contracts/src/schema_bundle.json",
    JSON.stringify(schemas),
  );
  write(root, "packages/contracts/src/schemas.ts", "export {};\n");
  write(
    root,
    "rust/crates/contracts/src/routes.rs",
    'pub const CONTRACT_VERSION: &str = "0.7.0";\n',
  );
  write(root, "rust/crates/host/src/lib.rs", '"contractVersion": "0.7.0"\n');
  write(
    root,
    "rust/crates/host/src/http/management.rs",
    '"contractVersion":"0.7.0"\n',
  );
  return root;
}
afterEach(() => {
  for (const directory of directories.splice(0))
    rmSync(directory, { recursive: true, force: true });
});

test("candidate fixtures preserve mirrors, schemas and hashes under the selected version", () => {
  const root = fixture();
  const paths = projectCandidateFixtures(root, "0.7.0", "0.6.0", "foundation");
  expect(paths).toContain("rust/crates/host/src/lib.rs");
  const manifest = JSON.parse(read(root, "rust/fixtures/manifest.json"));
  expect(manifest.contractVersion).toBe("0.6.0");
  expect(manifest.schemaCount).toBe(2);
  for (const [path, digest] of Object.entries(manifest.sha256)) {
    expect(sha256(read(root, `${roots[0]}/${path}`))).toBe(digest);
    expect(read(root, `${roots[0]}/${path}`)).toBe(
      read(root, `${roots[1]}/${path}`),
    );
  }
  const schemas = JSON.parse(read(root, "rust/fixtures/schemas.json"));
  expect(schemas.AiProviderContinuationSchema).toBeUndefined();
  expect(schemas.ExampleSchema.properties.contractVersion.const).toBe("0.6.0");
  expect(schemas.VersionDtoSchema.properties.contractVersion).toEqual({
    type: "string",
  });
  expect(
    JSON.parse(read(root, "rust/crates/contracts/src/schema_bundle.json")),
  ).toEqual(schemas);
});

test("changed fixtures and duplicate native version constants fail before projection writes", () => {
  for (const change of ["fixture", "native"] as const) {
    const root = fixture();
    const before = read(root, "rust/fixtures/manifest.json");
    if (change === "fixture")
      write(root, "rust/fixtures/run-events.json", "{}\n");
    else
      write(
        root,
        "rust/crates/host/src/lib.rs",
        '"contractVersion": "0.7.0"\n"contractVersion": "0.7.0"\n',
      );
    expect(() =>
      projectCandidateFixtures(root, "0.7.0", "0.6.0", "foundation"),
    ).toThrow(
      change === "fixture" ? "fixture hash drift" : "native version drift",
    );
    expect(read(root, "rust/fixtures/manifest.json")).toBe(before);
  }
});
