#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { sha256 } from "./foundation.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));
const destination = resolve(process.argv[2] ?? "candidate");
const sourceOnly = process.env.CANDIDATE_SOURCE_ONLY === "1";
const electron = process.env.ELECTRON_VERSION;
const track = process.env.CANDIDATE_TRACK ?? "foundation";
if (!["foundation", "responses"].includes(track))
  throw new Error("Unknown candidate track");
if (existsSync(destination)) throw new Error("Candidate output must be new");
mkdirSync(destination, { recursive: true });

function command(executable, args, cwd, evidence, name, checks) {
  const result = spawnSync(executable, args, {
    cwd,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  writeFileSync(
    join(evidence, `${name}.log`),
    `${result.stdout ?? ""}${result.stderr ?? ""}`,
  );
  checks.push({
    name,
    command: [executable, ...args],
    status: result.status,
    passed: result.status === 0,
  });
  console.log(`${name}: ${result.status === 0 ? "passed" : "failed"}`);
  if (result.status !== 0) throw new Error(`${name} failed; see ${name}.log`);
  return result.stdout;
}

function archive(source, evidence, manifest, checks) {
  const files = join(evidence, "source-files.list");
  writeFileSync(
    files,
    `${manifest.files.map((file) => file.path).join("\0")}\0`,
  );
  command(
    "tar",
    [
      "-czf",
      join(evidence, "source.tar.gz"),
      "-C",
      source,
      "--null",
      "-T",
      files,
    ],
    source,
    evidence,
    "source-archive",
    checks,
  );
}

function sourceGates(source, evidence, checks) {
  for (const [name, executable, args] of [
    ["install", "bun", ["install", "--frozen-lockfile"]],
    ["typecheck", "bun", ["run", "typecheck"]],
    ["lint", "bun", ["run", "lint"]],
    ["tests", "bun", ["test"]],
    ["build", "bun", ["run", "build"]],
    ["umbrella", "bun", ["run", "build:umbrella"]],
    ["node-smoke", "node", ["scripts/node-smoke.mjs"]],
    [
      "node-sqlite",
      "node",
      [
        "--test",
        "scripts/node-sqlite-conformance.mjs",
        "scripts/sqlite-durability.mjs",
      ],
    ],
    [
      "bun-sqlite",
      "env",
      [
        "AGENTKIT_SQLITE_DRIVER=bun",
        "bun",
        "test",
        "./scripts/sqlite-durability.mjs",
      ],
    ],
  ])
    command(executable, args, source, evidence, name, checks);
}

function pruneDependencies(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const path = join(directory, entry.name);
    if (
      ["node_modules", "npm-cache", "bun-cache", "electron-cache"].includes(
        entry.name,
      )
    )
      rmSync(path, { recursive: true });
    else pruneDependencies(path);
  }
}

function captureSource(state) {
  const { source, evidence, release, version, checks } = state;
  command(
    "node",
    [join(root, "scripts/snapshot-release.mjs"), source, version, release],
    root,
    evidence,
    "snapshot",
    checks,
  );
  command(
    "node",
    [
      "scripts/source-digest.mjs",
      source,
      join(evidence, "source-manifest.json"),
    ],
    source,
    evidence,
    "source-before",
    checks,
  );
  state.manifest = JSON.parse(
    readFileSync(join(evidence, "source-manifest.json")),
  );
  state.provenance = JSON.parse(
    readFileSync(join(source, "snapshot-provenance.json")),
  );
  writeFileSync(
    join(evidence, "snapshot-provenance.json"),
    readFileSync(join(source, "snapshot-provenance.json")),
  );
  if (
    process.env.SOURCE_COMMIT &&
    process.env.SOURCE_COMMIT !== state.provenance.baseline
  )
    throw new Error("Workflow source differs from snapshot baseline");
  writeFileSync(
    join(evidence, "dirty.patch"),
    readFileSync(`${source}.dirty.patch`),
  );
  archive(source, evidence, state.manifest, checks);
}

function assertStableSource(state) {
  const { source, evidence, manifest, checks } = state;
  command(
    "node",
    [
      "scripts/source-digest.mjs",
      source,
      join(evidence, "source-manifest-after.json"),
    ],
    source,
    evidence,
    "source-after",
    checks,
  );
  const after = JSON.parse(
    readFileSync(join(evidence, "source-manifest-after.json")),
  );
  if (manifest.sha256 !== after.sha256)
    throw new Error("Source changed during qualification");
}

function packAndQualify(state, foundation) {
  const { source, evidence, manifest, release, version, checks } = state;
  const pack = JSON.parse(
    command(
      "npm",
      ["pack", "./packages/agentkit", "--pack-destination", evidence, "--json"],
      source,
      evidence,
      "pack",
      checks,
    ),
  );
  writeFileSync(join(evidence, "pack.json"), JSON.stringify(pack, null, 2));
  const tarball = join(evidence, `agentkit-${version}.tgz`);
  if (pack.length !== 1 || pack[0].filename !== `agentkit-${version}.tgz`)
    throw new Error("Packed artifact does not match expected version");
  const args = [
    "scripts/qualify-package.mjs",
    "--tarball",
    tarball,
    "--source-digest",
    manifest.sha256,
    "--output",
    join(evidence, "qualification"),
  ];
  if (release === "responses")
    args.push("--responses", "true", "--migration-from", foundation);
  if (electron) args.push("--electron", electron);
  command("node", args, source, evidence, "package-qualification", checks);
  pruneDependencies(join(evidence, "qualification"));
  command(
    "tar",
    [
      "-czf",
      join(evidence, "qualification-evidence.tar.gz"),
      "-C",
      evidence,
      "qualification",
    ],
    source,
    evidence,
    "evidence-archive",
    checks,
  );
  return tarball;
}

function recordCandidate(state, tarball) {
  const { evidence, manifest, provenance, release, version } = state;
  const candidate = {
    format: "agentkit-qualified-candidate-v1",
    release,
    version,
    track,
    sourceCommit: provenance.baseline,
    sourceRef: process.env.SOURCE_REF ?? null,
    qualificationRun: process.env.GITHUB_RUN_ID ?? null,
    sourceDigest: manifest.sha256,
    tarball: `agentkit-${version}.tgz`,
    tarballSha256: sha256(readFileSync(tarball)),
    sourceArchiveSha256: sha256(readFileSync(join(evidence, "source.tar.gz"))),
    evidenceArchiveSha256: sha256(
      readFileSync(join(evidence, "qualification-evidence.tar.gz")),
    ),
    platform: process.platform,
    arch: process.arch,
    node: process.version,
    bun: spawnSync("bun", ["--version"], { encoding: "utf8" }).stdout.trim(),
    ...(electron ? { electron } : {}),
    passed: true,
  };
  writeFileSync(
    join(evidence, "candidate.json"),
    `${JSON.stringify(candidate, null, 2)}\n`,
  );
}

function qualify(release, version, foundation) {
  const source = join(destination, `${release}-source`);
  const evidence = join(destination, release);
  const checks = [];
  const state = { source, evidence, release, version, checks };
  mkdirSync(evidence);
  try {
    captureSource(state);
    sourceGates(source, evidence, checks);
    assertStableSource(state);
    if (sourceOnly) return undefined;
    const tarball = packAndQualify(state, foundation);
    recordCandidate(state, tarball);
    return tarball;
  } finally {
    writeFileSync(
      join(evidence, "source-gates.json"),
      `${JSON.stringify({ release, version, sourceOnly, checks }, null, 2)}\n`,
    );
  }
}

const foundation = qualify("foundation", "0.6.0");
if (track === "responses") qualify("responses", "0.7.0", foundation);
