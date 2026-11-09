#!/usr/bin/env node
// Public-API snapshot gate: `cargo public-api` output per library crate lives in
// rust/api/<package>.txt. Default mode diffs against it; --update rewrites it.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const rustRoot = resolve(scriptDir, "..");
const apiDir = join(rustRoot, "api");
const update = process.argv.includes("--update");

function run(cmd, args) {
  const result = spawnSync(cmd, args, {
    cwd: rustRoot,
    encoding: "utf8",
    maxBuffer: 256 * 1024 * 1024,
  });
  if (result.status !== 0) {
    return {
      ok: false,
      out: `${result.stderr ?? ""}${result.error ?? ""}`.trim(),
    };
  }
  return { ok: true, out: result.stdout };
}

function libraryCrates() {
  const meta = run("cargo", [
    "metadata",
    "--format-version",
    "1",
    "--no-deps",
    "--locked",
  ]);
  if (!meta.ok) {
    console.error(`public-api: cargo metadata failed\n${meta.out}`);
    process.exit(2);
  }
  const cratesRoot = `${join(rustRoot, "crates")}/`;
  return JSON.parse(meta.out)
    .packages.filter((pkg) => pkg.manifest_path.startsWith(cratesRoot))
    .filter((pkg) =>
      pkg.targets.some((t) =>
        t.kind.some((k) => k === "lib" || k === "cdylib"),
      ),
    )
    .map((pkg) => ({ name: pkg.name, manifest: pkg.manifest_path }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

function lineDiff(expected, actual) {
  const want = new Set(expected.split("\n"));
  const have = new Set(actual.split("\n"));
  const removed = expected.split("\n").filter((line) => !have.has(line));
  const added = actual.split("\n").filter((line) => !want.has(line));
  return [...removed.map((l) => `- ${l}`), ...added.map((l) => `+ ${l}`)];
}

mkdirSync(apiDir, { recursive: true });
const failures = [];
for (const { name, manifest } of libraryCrates()) {
  const listed = run("cargo", [
    "+nightly",
    "public-api",
    "--manifest-path",
    manifest,
    "--all-features",
    "-sss",
    "--color",
    "never",
  ]);
  if (!listed.ok) {
    failures.push(`${name}: cargo public-api failed\n${listed.out}`);
    continue;
  }
  const file = join(apiDir, `${name}.txt`);
  if (update) {
    writeFileSync(file, listed.out);
    console.log(`public-api: wrote ${name}`);
    continue;
  }
  const committed = existsSync(file) ? readFileSync(file, "utf8") : null;
  if (committed === null) {
    failures.push(
      `${name}: no baseline at rust/api/${name}.txt (run --update)`,
    );
  } else if (committed !== listed.out) {
    const diff = lineDiff(committed, listed.out);
    failures.push(
      `${name}: public API changed (run --update if intended)\n${diff.join("\n")}`,
    );
  }
}

if (failures.length > 0) {
  console.error(
    `public-api: ${failures.length} problem(s)\n\n${failures.join("\n\n")}`,
  );
  process.exit(1);
}
console.log(update ? "public-api: updated" : "public-api: ok");
