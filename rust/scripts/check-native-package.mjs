#!/usr/bin/env node
/**
 * Packed-native-package install gate.
 *
 * `bun test` and `cargo test` exercise `@agentkit/native` from the workspace
 * checkout, where the loader and the `.node` binary sit next to the sources
 * and a stale local build can mask packaging bugs. This gate exercises what a
 * consumer actually gets: it builds the package, `npm pack`s it, `npm
 * install`s the resulting tarball into an isolated temp directory OUTSIDE the
 * repository, and runs one full scripted turn (in-memory `NativeStore` →
 * service → session → send → drain events → close) under BOTH bun and plain
 * node, asserting the loader resolves with no `ERR_MODULE_NOT_FOUND`.
 *
 * Conventions mirror `rust/scripts/check-packages.mjs` and
 * `scripts/pack-smoke.mjs`: per-run temp directories (removed in `finally`),
 * loud failures that echo the failing command's output, no silent skips.
 *
 * Two deliberate design points:
 *
 *  - The pack runs on a staging COPY inside the temp dir (plus a copy of
 *    `scripts/` so the package's `prepack`/`postpack` publishConfig swap
 *    resolves). Packing the live `packages/native` directory would let the
 *    repository `.gitignore` (which ignores `generated/*.node`) drop the
 *    binary from the tarball, and would rewrite the real `package.json`
 *    mid-run; the copy avoids both and leaves the working tree untouched.
 *  - `npm install` and both consumer runs execute under a PATH stripped of
 *    cargo/rustup/rustc/toolchain locations and shadowed by a stub bin
 *    holding non-executable `cargo`/`rustc`/`rustup` entries, and the script
 *    asserts — BEFORE installing — that spawning `cargo` (and `rustc`) under
 *    that PATH dies with a spawn error. The packed loader must resolve the
 *    addon from the shipped binary; if a toolchain were reachable, a
 *    build-on-demand loader regression could pass this gate unnoticed.
 *
 * `AGENTKIT_NATIVE_PACK_SKIP_BUILD=1` skips the two build commands and packs
 * the artifacts as they currently are. That is the knob the mutation drill
 * uses (delete `generated/index.js`, run with the flag, the gate MUST fail
 * non-zero); CI never sets it.
 */
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const nativePackage = join(repoRoot, "packages", "native");
const skipBuild = process.env.AGENTKIT_NATIVE_PACK_SKIP_BUILD === "1";
const home = homedir();

function fail(message) {
  throw new Error(`check-native-package: ${message}`);
}

function run(command, arguments_, options = {}) {
  const { capture = false, ...spawnOptions } = options;
  const result = spawnSync(command, arguments_, {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    stdio: capture ? "pipe" : "inherit",
    ...spawnOptions,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (capture)
      process.stderr.write(`${result.stdout ?? ""}${result.stderr ?? ""}`);
    throw new Error(
      `check-native-package: ${command} ${arguments_.join(" ")} failed with status ${result.status}`,
    );
  }
  return result.stdout ?? "";
}

function probe(command, arguments_) {
  return spawnSync(command, arguments_, {
    encoding: "utf8",
    stdio: "ignore",
  });
}

/** First candidate that spawns `--version` successfully; missing ones skipped. */
function locate(candidates, label) {
  for (const candidate of candidates) {
    if (candidate && !probe(candidate, ["--version"]).error) return candidate;
  }
  fail(`no usable ${label}; tried: ${candidates.filter(Boolean).join(", ")}`);
}

const bunBinary = locate(
  ["bun", join(home, ".bun/bin/bun")],
  "bun binary (bun on PATH or ~/.bun/bin/bun)",
);
const nodeBinary = locate(
  [
    "node",
    join(home, ".local/bin/node"),
    process.execPath, // the gate itself runs under node; it can host itself
  ],
  "node binary (node on PATH or ~/.local/bin/node)",
);

/** Absolute path of a node-family binary's real distribution bin directory
 * (follows symlinks: ~/.local/bin/node → ~/.local/lib/node-vX/bin/node). */
function realBinDir(binary) {
  const result = spawnSync(
    binary,
    ["-e", "process.stdout.write(process.execPath)"],
    { encoding: "utf8" },
  );
  if (result.error || !result.stdout) return undefined;
  return dirname(result.stdout.trim());
}

const npmBinary =
  (() => {
    const candidates = [
      "npm",
      realBinDir(nodeBinary) && join(realBinDir(nodeBinary), "npm"),
      realBinDir(process.execPath) && join(realBinDir(process.execPath), "npm"),
    ].filter((candidate) => candidate && existsSync(candidate));
    return candidates.find(
      (candidate) => !probe(candidate, ["--version"]).error,
    );
  })() ??
  fail(
    "no usable npm binary; tried `npm` on PATH and the npm next to the " +
      "resolved node distributions",
  );

// Per-run temporary directories, both outside the repository.
const temporary = mkdtempSync(join(tmpdir(), "agentkit-native-pack-"));
const consumerDirectory = mkdtempSync(
  join(tmpdir(), "agentkit-native-consumer-"),
);

try {
  // -------------------------------------------------------------------------
  // Toolchain-free environment for the install + consumer runs.
  // -------------------------------------------------------------------------
  const stubDirectory = join(temporary, "toolchain-stub-bin");
  mkdirSync(stubDirectory, { recursive: true });
  for (const tool of ["cargo", "rustc", "rustup"]) {
    const stub = join(stubDirectory, tool);
    writeFileSync(stub, "");
    chmodSync(stub, 0o000);
  }
  // A bare PATH name like "bun" has no directory to add (dirname("bun") is
  // "." — cwd pollution); only absolute installs contribute their bin dir.
  const binDir = (binary) => (isAbsolute(binary) ? dirname(binary) : undefined);
  const toolDirectories = [];
  for (const directory of [
    realBinDir(nodeBinary),
    binDir(bunBinary),
    binDir(npmBinary),
    join(home, ".local/bin"),
    join(home, ".bun/bin"),
  ]) {
    if (
      directory &&
      existsSync(directory) &&
      !toolDirectories.includes(directory)
    )
      toolDirectories.push(directory);
  }
  const systemPath = (process.env.PATH ?? "")
    .split(delimiter)
    .filter(
      (entry) => entry !== "" && !/cargo|rustup|rustc|toolchain/i.test(entry),
    )
    // Directory names lie: `/usr/bin/cargo` is a real apt-installed toolchain
    // on many images. Drop every remaining entry that actually holds one —
    // glibc's execvp walks past non-executable stubs, so shadowing alone is
    // not enough; the binaries must be off the search path entirely.
    .filter(
      (entry) =>
        !["cargo", "rustc", "rustup"].some((tool) =>
          existsSync(join(entry, tool)),
        ),
    );
  const toolchainFreeEnvironment = {
    ...process.env,
    PATH: [stubDirectory, ...toolDirectories, ...systemPath].join(delimiter),
  };

  function assertToolchainAbsent(tool) {
    const result = spawnSync(tool, ["--version"], {
      env: toolchainFreeEnvironment,
      stdio: "ignore",
    });
    if (!result.error)
      fail(
        `\`${tool}\` was reachable under the toolchain-free PATH (spawn ` +
          "succeeded) — the packed loader must be proven binary-backed, so " +
          "the install and consumer runs may not see a Rust toolchain. " +
          `Stub directory: ${stubDirectory}`,
      );
  }

  // -------------------------------------------------------------------------
  // 1. Build the package artifacts (dist + generated loader and addon).
  // -------------------------------------------------------------------------
  if (skipBuild) {
    process.stdout.write(
      "check-native-package: AGENTKIT_NATIVE_PACK_SKIP_BUILD=1 — packing " +
        "the existing artifacts as-is (mutation drill / fast rerun mode).\n",
    );
  } else {
    // `build` runs tsc and `build:native` runs napi (cargo) — both under the
    // full environment; only the INSTALL and CONSUMER runs are cargo-free.
    const buildEnvironment = {
      ...process.env,
      PATH: [
        realBinDir(nodeBinary),
        binDir(bunBinary),
        binDir(npmBinary),
        join(home, ".bun/bin"),
        join(home, ".local/bin"),
        ...(process.env.PATH ?? "").split(delimiter).filter(Boolean),
      ]
        .filter(Boolean)
        .join(delimiter),
    };
    run(bunBinary, ["run", "--cwd", "packages/native", "build"], {
      env: buildEnvironment,
    });
    run(bunBinary, ["run", "--cwd", "packages/native", "build:native"], {
      env: buildEnvironment,
    });
  }

  // -------------------------------------------------------------------------
  // 2. Stage a copy outside the repository and `npm pack` it into the temp dir.
  // -------------------------------------------------------------------------
  const staging = join(temporary, "stage");
  const stagedPackage = join(staging, "packages/native");
  mkdirSync(join(staging, "packages"), { recursive: true });
  cpSync(nativePackage, stagedPackage, { recursive: true });
  cpSync(join(repoRoot, "scripts"), join(staging, "scripts"), {
    recursive: true,
  });

  const manifest = JSON.parse(
    readFileSync(join(stagedPackage, "package.json"), "utf8"),
  );
  // `npm pack` output filenames follow the pack-smoke convention:
  // `@scope/name@version` → `scope-name-version.tgz`.
  const tarballName = `${manifest.name.replace(/^@/, "").replace("/", "-")}-${manifest.version}.tgz`;
  const tarballs = join(temporary, "tarballs");
  mkdirSync(tarballs, { recursive: true });
  run(npmBinary, ["pack", "--pack-destination", tarballs], {
    cwd: stagedPackage,
  });
  const tarball = join(tarballs, tarballName);
  if (!existsSync(tarball))
    fail(`npm pack did not produce ${tarballName} in ${tarballs}`);

  // Loud, specific pre-install check of the archive contents: a missing
  // generated loader must fail HERE, before any install or import can hide
  // it behind a generic resolution error.
  const archiveList = run("tar", ["-tzf", tarball], { capture: true })
    .split("\n")
    .filter((entry) => entry !== "");
  const missing = [
    "package/package.json",
    "package/dist/index.js",
    "package/generated/index.js",
    "package/generated/index.d.ts",
  ].filter((entry) => !archiveList.includes(entry));
  if (
    !archiveList.some((entry) => /^package\/generated\/.+\.node$/.test(entry))
  )
    missing.push("package/generated/<platform>.node (the native addon)");
  if (missing.length > 0)
    fail(
      `the packed tarball ${tarballName} is missing ${missing.join(", ")}.\n` +
        `Archive contents:\n${archiveList.map((entry) => `  ${entry}`).join("\n")}`,
    );
  const packedManifest = JSON.parse(
    run("tar", ["-xzOf", tarball, "package/package.json"], { capture: true }),
  );
  if (
    packedManifest.main !== "./dist/index.js" ||
    packedManifest.exports?.["."]?.import !== "./dist/index.js"
  )
    fail(
      `the packed package.json still points at non-dist entry points ` +
        `(main=${packedManifest.main}, exports=${JSON.stringify(packedManifest.exports)}); ` +
        "the prepack publishConfig swap did not run",
    );
  if (JSON.stringify(packedManifest).includes("workspace:"))
    fail("packed manifest contains a workspace: dependency range");

  // -------------------------------------------------------------------------
  // 3. Isolated install + consumer turn under bun AND node, cargo-free.
  // -------------------------------------------------------------------------
  assertToolchainAbsent("cargo");
  assertToolchainAbsent("rustc");

  writeFileSync(
    join(consumerDirectory, "package.json"),
    `${JSON.stringify(
      { name: "agentkit-native-pack-consumer", private: true, type: "module" },
      null,
      2,
    )}\n`,
  );

  // The acceptance scenario lives in a real file —
  // `rust/scripts/native-package-consumer.mjs` — copied into the isolated
  // consumer directory and driven there as SEPARATE child processes per
  // mode, importing the INSTALLED package by name. Fresh child processes
  // give the reopen phase a true fresh-process replay.
  cpSync(
    join(repoRoot, "rust", "scripts", "native-package-consumer.mjs"),
    join(consumerDirectory, "consumer.mjs"),
  );

  run(npmBinary, ["install", "--no-audit", "--no-fund", tarball], {
    cwd: consumerDirectory,
    env: toolchainFreeEnvironment,
    timeout: 300_000,
  });
  if (!existsSync(join(consumerDirectory, "node_modules/@agentkit/native")))
    fail(
      "npm install did not place @agentkit/native in the isolated consumer directory",
    );

  for (const [label, runtime] of [
    ["bun", bunBinary],
    ["node", nodeBinary],
  ]) {
    const dbPath = join(consumerDirectory, `acceptance-${label}.db`);
    process.stdout.write(
      `check-native-package: acceptance lifecycle (approval round-trip, ` +
        `cancel, awaited close) under ${label}…\n`,
    );
    run(runtime, ["consumer.mjs", "run", dbPath], {
      cwd: consumerDirectory,
      env: toolchainFreeEnvironment,
      timeout: 120_000,
    });
    // The reopen phase is a SEPARATE child process: fresh-process replay
    // of the same durable database, with no provider and no tools.
    process.stdout.write(
      `check-native-package: fresh-process reopen + durable replay under ${label}…\n`,
    );
    run(runtime, ["consumer.mjs", "reopen", dbPath], {
      cwd: consumerDirectory,
      env: toolchainFreeEnvironment,
      timeout: 120_000,
    });
  }

  process.stdout.write(
    `check-native-package: packed ${tarballName} installed isolated and ran ` +
      "the full acceptance lifecycle (send → committed events → approval " +
      "wait → answer → resume → terminal completion → cancel → awaited close) " +
      "plus a fresh-process reopen with durable replay, under bun and node " +
      "with no reachable toolchain.\n",
  );
} finally {
  rmSync(temporary, { recursive: true, force: true });
  rmSync(consumerDirectory, { recursive: true, force: true });
}
