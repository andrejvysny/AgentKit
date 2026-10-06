#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, relative, resolve } from "node:path";

const [directory, destination] = process.argv.slice(2);
if (!directory || !destination)
  throw new Error(
    "Usage: node scripts/source-digest.mjs SOURCE_DIR OUTPUT_JSON",
  );
const source = resolve(directory);
const output = resolve(destination);
const ignored = new Set([
  ".git",
  "node_modules",
  "dist",
  "coverage",
  ".DS_Store",
]);
const files = [];

function walk(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (ignored.has(entry.name) || entry.name.endsWith(".tsbuildinfo"))
      continue;
    const path = join(directory, entry.name);
    if (path === output) continue;
    if (entry.isSymbolicLink())
      throw new Error(`Source symlink requires explicit review: ${path}`);
    if (entry.isDirectory()) walk(path);
    else
      files.push({
        path: relative(source, path),
        sha256: createHash("sha256").update(readFileSync(path)).digest("hex"),
      });
  }
}

walk(source);
files.sort((left, right) =>
  left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
);
const sha256 = createHash("sha256").update(JSON.stringify(files)).digest("hex");
writeFileSync(
  output,
  `${JSON.stringify(
    {
      algorithm: "sha256-sorted-source-manifest-v1",
      excludes: [...ignored, "*.tsbuildinfo", "OUTPUT_JSON"],
      sha256,
      files,
    },
    null,
    2,
  )}\n`,
);
console.log(sha256);
