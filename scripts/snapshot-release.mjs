#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  foundationExclusions,
  projectFoundation,
  sha256,
} from "./release/foundation.mjs";

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
const outputRelative = relative(root, output);
if (
  !(
    outputRelative === ".." ||
    outputRelative.startsWith("../") ||
    isAbsolute(outputRelative)
  ) ||
  existsSync(output)
) {
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

const baseline = git(["rev-parse", "HEAD"]).trim();
const dirtyPatch = git(["diff", "--binary", "HEAD"]);
function sourcePaths() {
  return [
    ...new Set(
      git(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .split("\0")
        .filter((path) => path && existsSync(join(root, path))),
    ),
  ].sort();
}
const sources = sourcePaths();
const original = [];
mkdirSync(output, { recursive: true });
for (const path of [...new Set(sources)].sort()) {
  const input = join(root, path);
  if (!lstatSync(input).isFile())
    throw new Error(`Snapshot source must be a regular file: ${path}`);
  const bytes = readFileSync(input);
  original.push({ path, sha256: sha256(bytes) });
  mkdirSync(dirname(join(output, path)), { recursive: true });
  copyFileSync(input, join(output, path));
  if (sha256(readFileSync(join(output, path))) !== sha256(bytes))
    throw new Error(`Copy raced: ${path}`);
}
if (JSON.stringify(sources) !== JSON.stringify(sourcePaths()))
  throw new Error("Source file listing changed during snapshot");
for (const file of original) {
  if (sha256(readFileSync(join(root, file.path))) !== file.sha256)
    throw new Error(`Source changed during snapshot: ${file.path}`);
}
if (
  baseline !== git(["rev-parse", "HEAD"]).trim() ||
  dirtyPatch !== git(["diff", "--binary", "HEAD"])
)
  throw new Error("Git source changed during snapshot");

function rewrite(path, transform) {
  const file = join(output, path);
  writeFileSync(file, transform(readFileSync(file, "utf8")));
}

function replaceExactly(text, pattern, replacement, label) {
  const matches = [...text.matchAll(new RegExp(pattern.source, "g"))];
  if (matches.length !== 1)
    throw new Error(`Expected one ${label}, found ${matches.length}`);
  return text.replace(pattern, replacement);
}

const patches = release === "foundation" ? projectFoundation(output) : [];
rewrite("packages/contracts/src/version.ts", (text) =>
  replaceExactly(
    text,
    /export const CONTRACT_VERSION = "[^"]+";/,
    `export const CONTRACT_VERSION = "${version}";`,
    "contract version",
  ),
);
const versionedFixtures = readdirSync(
  join(output, "packages/testing/src/golden/traces"),
)
  .filter((path) => path.endsWith(".json"))
  .sort()
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
  replaceExactly(
    text,
    /("packages\/agentkit": \{\s*"name": "agentkit",\s*"version": ")[^"]+/,
    `$1${version}`,
    "umbrella workspace lock version",
  ),
);
const excluded = release === "foundation" ? foundationExclusions : [];
const changedFiles = original.flatMap((file) => {
  if (excluded.includes(file.path)) return [];
  const after = sha256(readFileSync(join(output, file.path)));
  return after === file.sha256
    ? []
    : [{ path: file.path, before: file.sha256, after }];
});
writeFileSync(
  join(output, "snapshot-provenance.json"),
  `${JSON.stringify(
    {
      format: "agentkit-source-projection-v1",
      release,
      version,
      baseline,
      dirtyPatchSha256: sha256(dirtyPatch),
      originalFiles: original,
      requestedExclusions: excluded,
      excluded: original.filter((file) => excluded.includes(file.path)),
      patches,
      changedFiles,
      versionedFixtures,
    },
    null,
    2,
  )}\n`,
);
writeFileSync(`${output}.dirty.patch`, dirtyPatch);
console.log(`Frozen ${release} candidate ${version}: ${output}`);
