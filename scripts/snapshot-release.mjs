#!/usr/bin/env node
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const [destination, version, release] = process.argv.slice(2);
if (
  !destination ||
  !/^\d+\.\d+\.\d+$/.test(version ?? "") ||
  !["foundation", "responses"].includes(release)
) {
  throw new Error(
    "Usage: node scripts/snapshot-release.mjs NEW_DIRECTORY VERSION foundation|responses",
  );
}
const output = resolve(destination);
if (
  release === "foundation" &&
  existsSync(join(root, "packages/host/src/turn/provider-continuation.ts"))
) {
  throw new Error(
    "Foundation snapshot must precede provider continuation integration. Use the captured schema8 foundation source and reviewed foundation-only patches.",
  );
}
if (!relative(root, output).startsWith("..") || existsSync(output)) {
  throw new Error("Snapshot must use a new directory outside the checkout");
}

function git(args) {
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.status !== 0) throw new Error(result.stderr || "Git read failed");
  return result.stdout;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

const excluded =
  release === "foundation"
    ? [
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
        "packages/contracts/src/provider-continuation.ts",
      ]
    : [];
const baseline = git(["rev-parse", "HEAD"]).trim();
const dirtyPatch = git(["diff", "--binary", "HEAD"]);
const sources = git([
  "ls-files",
  "--cached",
  "--others",
  "--exclude-standard",
  "-z",
])
  .split("\0")
  .filter(
    (path) => path && !excluded.includes(path) && existsSync(join(root, path)),
  );
mkdirSync(output, { recursive: true });
const original = [];
for (const path of [...new Set(sources)].sort()) {
  const input = join(root, path);
  const bytes = readFileSync(input);
  original.push({ path, sha256: sha256(bytes) });
  mkdirSync(dirname(join(output, path)), { recursive: true });
  copyFileSync(input, join(output, path));
  if (sha256(readFileSync(join(output, path))) !== sha256(bytes))
    throw new Error(`Copy raced: ${path}`);
}
for (const file of original) {
  if (sha256(readFileSync(join(root, file.path))) !== file.sha256)
    throw new Error(`Source changed during snapshot: ${file.path}`);
}

function rewrite(path, transform) {
  const file = join(output, path);
  writeFileSync(file, transform(readFileSync(file, "utf8")));
}

if (release === "foundation") {
  rewrite("packages/contracts/src/index.ts", (text) =>
    text.replace(/^export \* from "\.\/provider-continuation\.js";\n?/m, ""),
  );
  rewrite("packages/core/src/index.ts", (text) =>
    text.replace(
      /^export \* from "\.\/providers\/responses(?:-types)?\.js";\n?/gm,
      "",
    ),
  );
  rewrite("packages/contracts/src/schemas.ts", (text) =>
    text.replace(
      /^export \{ AiProviderContinuationSchema \} from "\.\/provider-continuation\.js";\n?/m,
      "",
    ),
  );
  rewrite("packages/core/src/providers/client.ts", (text) =>
    text
      .replace(/^\s*AiProviderContinuation,\n/m, "")
      .replace(
        /^ {2}\/\*\* Trusted host continuation; never populated from a public request body\. \*\/\n/m,
        "",
      )
      .replace(
        /^ {2}(?:continuation|continuationScope|continuationRequired|onContinuation)\?.*\n/gm,
        "",
      ),
  );
}
rewrite("packages/contracts/src/version.ts", (text) =>
  text.replace(
    /export const CONTRACT_VERSION = "[^"]+";/,
    `export const CONTRACT_VERSION = "${version}";`,
  ),
);
const versionedFixtures = readdirSync(
  join(output, "packages/testing/src/golden/traces"),
)
  .filter((path) => path.endsWith(".json"))
  .map((path) => `packages/testing/src/golden/traces/${path}`);
for (const path of versionedFixtures)
  rewrite(path, (text) => {
    const events = JSON.parse(text);
    for (const event of events) event.contractVersion = version;
    return `${JSON.stringify(events, null, 2)}\n`;
  });
rewrite("packages/agentkit/package.json", (text) => {
  const manifest = JSON.parse(text);
  manifest.version = version;
  return `${JSON.stringify(manifest, null, 2)}\n`;
});
rewrite("bun.lock", (text) =>
  text.replace(
    /("packages\/agentkit": \{\s*"name": "agentkit",\s*"version": ")[^"]+/,
    `$1${version}`,
  ),
);
writeFileSync(
  join(output, "snapshot-provenance.json"),
  `${JSON.stringify(
    {
      release,
      version,
      baseline,
      dirtyPatchSha256: sha256(dirtyPatch),
      originalFiles: original,
      excluded,
      versionedFixtures,
      transformations:
        release === "foundation"
          ? [
              "Remove Responses barrel exports and schema registration",
              "Remove provider continuation type/request fields; retain onActivity",
              "Retain hardened generic SSE",
              "Set umbrella version and lock workspace metadata",
              "Set wire contract version",
            ]
          : [
              "Set umbrella version and lock workspace metadata",
              "Set wire contract version",
            ],
    },
    null,
    2,
  )}\n`,
);
writeFileSync(`${output}.dirty.patch`, dirtyPatch);
console.log(`Frozen ${release} candidate ${version}: ${output}`);
