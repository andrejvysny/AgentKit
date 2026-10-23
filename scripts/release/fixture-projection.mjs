import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { sha256 } from "./foundation.mjs";

function canonical(value) {
  const sort = (item) => {
    if (Array.isArray(item)) return item.map(sort);
    if (item !== null && typeof item === "object")
      return Object.fromEntries(
        Object.keys(item)
          .sort()
          .map((key) => [key, sort(item[key])]),
      );
    return item;
  };
  return `${JSON.stringify(sort(value), null, 2)}\n`;
}

function projectVersion(value, before, after) {
  if (Array.isArray(value))
    return value.map((item) => projectVersion(item, before, after));
  if (value === null || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, item]) => {
      if (key !== "contractVersion")
        return [key, projectVersion(item, before, after)];
      if (item === before) return [key, after];
      if (item && typeof item === "object") {
        if (!Object.hasOwn(item, "const")) return [key, item];
        if (item.const === before) return [key, { ...item, const: after }];
      }
      throw new Error("Candidate fixture contract-version drift");
    }),
  );
}

function nativeVersions(read, before, after) {
  const versions = new Map();
  for (const [path, literal] of [
    [
      "rust/crates/contracts/src/routes.rs",
      `pub const CONTRACT_VERSION: &str = "${before}";`,
    ],
    ["rust/crates/host/src/lib.rs", `"contractVersion": "${before}"`],
    [
      "rust/crates/host/src/http/management.rs",
      `"contractVersion":"${before}"`,
    ],
  ]) {
    const text = read(path);
    if (text.split(literal).length !== 2)
      throw new Error(`Candidate native version drift: ${path}`);
    versions.set(path, text.replace(literal, literal.replace(before, after)));
  }
  return versions;
}

/** Project captured metadata only; fixture bodies must match both committed manifests. */
export function projectCandidateFixtures(directory, before, after, release) {
  const roots = ["rust/fixtures", "rust/crates/contracts/tests/fixtures"];
  const read = (path) => readFileSync(join(directory, path), "utf8");
  const manifest = JSON.parse(read(`${roots[0]}/manifest.json`));
  const schemas = JSON.parse(read(`${roots[0]}/schemas.json`));
  if (
    manifest.contractVersion !== before ||
    manifest.schemaCount !== Object.keys(schemas).length
  )
    throw new Error("Candidate fixture manifest drift");
  for (const root of roots) {
    if (read(`${root}/manifest.json`) !== read(`${roots[0]}/manifest.json`))
      throw new Error("Candidate mirrored fixture manifest drift");
    for (const [path, digest] of Object.entries(manifest.sha256))
      if (sha256(read(`${root}/${path}`)) !== digest)
        throw new Error(`Candidate fixture hash drift: ${root}/${path}`);
  }
  if (
    canonical(
      JSON.parse(read("rust/crates/contracts/src/schema_bundle.json")),
    ) !== canonical(schemas)
  )
    throw new Error("Candidate native schema bundle drift");
  const schemaBarrel = read("packages/contracts/src/schemas.ts");
  const hasContinuation = schemaBarrel.includes(
    "export { AiProviderContinuationSchema }",
  );
  if (
    !Object.hasOwn(schemas, "AiProviderContinuationSchema") ||
    hasContinuation !== (release === "responses")
  )
    throw new Error("Candidate continuation schema projection drift");
  if (release === "foundation") delete schemas.AiProviderContinuationSchema;
  const bodies = new Map(
    Object.keys(manifest.sha256).map((path) => [
      path,
      canonical(
        projectVersion(
          path === "schemas.json"
            ? schemas
            : JSON.parse(read(`${roots[0]}/${path}`)),
          before,
          after,
        ),
      ),
    ]),
  );
  bodies.set(
    "manifest.json",
    canonical({
      ...manifest,
      contractVersion: after,
      schemaCount: Object.keys(schemas).length,
      sha256: Object.fromEntries(
        [...bodies].map(([path, body]) => [path, sha256(body)]),
      ),
    }),
  );
  const native = nativeVersions(read, before, after);
  const writes = [];
  for (const root of roots)
    for (const [path, body] of bodies) {
      writeFileSync(join(directory, root, path), body);
      writes.push(`${root}/${path}`);
    }
  const bundle = "rust/crates/contracts/src/schema_bundle.json";
  const nativeSchema = projectVersion(JSON.parse(read(bundle)), before, after);
  if (release === "foundation")
    delete nativeSchema.AiProviderContinuationSchema;
  writeFileSync(join(directory, bundle), `${JSON.stringify(nativeSchema)}\n`);
  writes.push(bundle);
  for (const [path, body] of native) {
    writeFileSync(join(directory, path), body);
    writes.push(path);
  }
  return writes;
}
