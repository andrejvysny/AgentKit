#!/usr/bin/env node
/**
 * Verify one packed native release artifact end-to-end.
 *
 *   node rust/scripts/check-native-artifact.mjs --manifest <artifact-manifest.<key>.json>
 *
 * Consumes the output of `build-native-release-artifact.mjs`: verifies
 * digests, inspects both tarballs (wrapper contents + publishConfig swap +
 * version lockstep + binary container identity), installs them offline into
 * an isolated consumer outside the repository, and — for a triple the host
 * can actually execute — drives the full acceptance lifecycle (approval
 * round-trip, cancel, awaited close, fresh-process reopen) under bun and
 * node with no Rust toolchain reachable. A `structuralOnly` artifact gets
 * the full static verification plus the honest missing-addon path (npm/bun
 * os/cpu gating must have kept the foreign binary out of the tree), and
 * says so in its verdict — a never-executed cell is never reported as
 * executed (M2.1 A4 rule).
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { assertBinaryMatchesTarget } from "./build-native-release-artifact.mjs";
import { nativeTargetByTriple } from "./native-platform-targets.mjs";
import {
  assertNoRust,
  findNpmCli,
  findRuntime,
  pathEntries,
  toolchainFreePath,
  withPath,
} from "./native-commands.mjs";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const home = homedir();

function fail(message) {
  throw new Error(`check-native-artifact: ${message}`);
}

const args = process.argv.slice(2);
const manifestFlag = args.indexOf("--manifest");
if (manifestFlag < 0 || !args[manifestFlag + 1])
  throw new Error(
    "usage: node rust/scripts/check-native-artifact.mjs --manifest <artifact-manifest.json>",
  );
const manifestPath = resolve(args[manifestFlag + 1]);
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
if (manifest.format !== "agentkit-native-artifact-v1")
  fail(`unexpected manifest format ${manifest.format}`);
const outDir = dirname(manifestPath);
const target = nativeTargetByTriple(manifest.triple);

const sha256 = (file) =>
  createHash("sha256").update(readFileSync(file)).digest("hex");
for (const [role, entry] of Object.entries(manifest.files)) {
  const file = join(outDir, entry.path);
  if (!existsSync(file)) fail(`${role} tarball missing: ${file}`);
  if (sha256(file) !== entry.sha256)
    fail(`${role} tarball sha256 mismatch: ${file}`);
  if (readFileSync(file).byteLength !== entry.bytes)
    fail(`${role} tarball byte length mismatch: ${file}`);
}
const wrapperTarball = join(outDir, manifest.files.wrapper.path);
const platformTarball = join(outDir, manifest.files.platform.path);

const paths = pathEntries(process.env.PATH ?? process.env.Path);
const bunBinary =
  findRuntime("bun", [...paths, join(home, ".bun/bin")]) ??
  fail("no usable bun binary");
const nodeBinary =
  findRuntime("node", [...paths, join(home, ".local/bin")]) ?? process.execPath;
const npmBinary = findNpmCli(nodeBinary, [...paths, dirname(process.execPath)]);

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
    fail(
      `${command} ${arguments_.join(" ")} failed with status ${result.status}`,
    );
  }
  return result.stdout ?? "";
}

const temporary = mkdtempSync(join(tmpdir(), "agentkit-native-artifact-"));
process.env.npm_config_cache = join(temporary, "npm-cache");
const consumerDirectory = mkdtempSync(
  join(tmpdir(), "agentkit-native-artifact-consumer-"),
);

try {
  const toolDirectories = [
    dirname(nodeBinary),
    dirname(bunBinary),
    dirname(npmBinary),
    join(home, ".local/bin"),
    join(home, ".bun/bin"),
  ].filter((directory) => directory && existsSync(directory));
  const cleanPath = toolchainFreePath([...new Set(toolDirectories), ...paths]);
  const toolchainFreeEnvironment = withPath(process.env, cleanPath);

  // --- Wrapper tarball contents -------------------------------------------
  const archiveList = run("tar", ["-tzf", wrapperTarball], { capture: true })
    .split("\n")
    .filter((entry) => entry !== "");
  const missing = [
    "package/package.json",
    "package/dist/index.js",
    "package/generated/index.js",
    "package/generated/index.d.ts",
  ].filter((entry) => !archiveList.includes(entry));
  if (archiveList.some((entry) => /\.node$/.test(entry)))
    fail("wrapper tarball unexpectedly includes a platform addon");
  if (missing.length > 0)
    fail(`wrapper tarball is missing ${missing.join(", ")}`);
  const packedWrapper = JSON.parse(
    run("tar", ["-xzOf", wrapperTarball, "package/package.json"], {
      capture: true,
    }),
  );
  if (packedWrapper.name !== manifest.wrapperPackage)
    fail(
      `wrapper tarball is ${packedWrapper.name}, manifest says ${manifest.wrapperPackage}`,
    );
  if (packedWrapper.version !== manifest.version)
    fail(
      `wrapper tarball version ${packedWrapper.version} != manifest ${manifest.version}`,
    );
  if (
    packedWrapper.main !== "./dist/index.js" ||
    packedWrapper.exports?.["."]?.import !== "./dist/index.js"
  )
    fail("prepack publishConfig swap did not run on the wrapper");
  if (JSON.stringify(packedWrapper).includes("workspace:"))
    fail("packed wrapper manifest contains a workspace: dependency range");
  if (packedWrapper.publishConfig?.access !== "public")
    fail("packed wrapper is not publish-configured public (scoped package)");
  const loader = run(
    "tar",
    ["-xzOf", wrapperTarball, "package/generated/index.js"],
    {
      capture: true,
    },
  );
  const checks = [...loader.matchAll(/bindingPackageVersion !== '([^']+)'/g)];
  if (checks.length === 0) fail("loader has no version checks");
  for (const check of checks)
    if (check[1] !== manifest.version)
      fail(
        `loader pins platform version ${check[1]}, artifact is ${manifest.version} — version lockstep broken`,
      );

  // --- Platform tarball contents ------------------------------------------
  const platformArchive = run("tar", ["-tzf", platformTarball], {
    capture: true,
  }).split("\n");
  const binaryEntry = `package/${manifest.files.platform.binary}`;
  if (!platformArchive.includes(binaryEntry))
    fail(`platform tarball missing ${binaryEntry}`);
  const packedPlatform = JSON.parse(
    run("tar", ["-xzOf", platformTarball, "package/package.json"], {
      capture: true,
    }),
  );
  if (packedPlatform.name !== manifest.package)
    fail(
      `platform tarball is ${packedPlatform.name}, manifest says ${manifest.package}`,
    );
  if (packedPlatform.version !== manifest.version)
    fail(
      `platform tarball version ${packedPlatform.version} != wrapper ${manifest.version}`,
    );
  if (packedPlatform.main !== manifest.files.platform.binary)
    fail(`platform main ${packedPlatform.main} != staged binary`);
  if (packedPlatform.private !== undefined)
    fail("platform package is still private — npm publish would refuse it");
  if (packedPlatform.publishConfig?.access !== "public")
    fail("platform package is not publish-configured public (scoped package)");
  if (JSON.stringify(packedPlatform.os) !== JSON.stringify([target.os]))
    fail(
      `platform os gating ${JSON.stringify(packedPlatform.os)} != ${target.os}`,
    );
  if (JSON.stringify(packedPlatform.cpu) !== JSON.stringify([target.cpu]))
    fail(
      `platform cpu gating ${JSON.stringify(packedPlatform.cpu)} != ${target.cpu}`,
    );
  const extractedBinary = join(
    temporary,
    "package",
    manifest.files.platform.binary,
  );
  run("tar", ["-xzf", platformTarball, "-C", temporary, binaryEntry], {
    capture: true,
  });
  if (!existsSync(extractedBinary))
    fail(`could not extract ${binaryEntry} from the platform tarball`);
  assertBinaryMatchesTarget(extractedBinary, target);

  // --- Isolated offline install --------------------------------------------
  assertNoRust(toolchainFreeEnvironment, cleanPath);
  writeFileSync(
    join(consumerDirectory, "package.json"),
    `${JSON.stringify(
      {
        name: "agentkit-native-artifact-consumer",
        private: true,
        type: "module",
        dependencies: { "@agentkit/native": `file:${wrapperTarball}` },
        optionalDependencies: {
          [manifest.package]: `file:${platformTarball}`,
        },
      },
      null,
      2,
    )}\n`,
  );
  run(
    nodeBinary,
    [npmBinary, "install", "--offline", "--no-audit", "--no-fund"],
    {
      cwd: consumerDirectory,
      env: toolchainFreeEnvironment,
      timeout: 300_000,
    },
  );
  if (!existsSync(join(consumerDirectory, "node_modules/@agentkit/native")))
    fail("npm install did not place @agentkit/native in the isolated consumer");
  const installedPlatformDir = join(
    consumerDirectory,
    "node_modules/@agentkit",
    `native-${target.key}`,
  );
  const addonInstalled = existsSync(
    join(installedPlatformDir, manifest.files.platform.binary),
  );

  cpSync(
    join(repoRoot, "rust", "scripts", "native-package-consumer.mjs"),
    join(consumerDirectory, "consumer.mjs"),
  );

  if (manifest.structuralOnly) {
    // os/cpu gating must have kept the foreign binary out of the tree…
    if (addonInstalled)
      fail(
        `structural-only artifact for ${manifest.triple} was installed on ${process.platform}-${process.arch}` +
          " — os/cpu gating is broken and the loader would try to load a foreign binary",
      );
    // …and the first-use path must fail loudly, never silently.
    run(
      nodeBinary,
      [
        "--input-type=module",
        "-e",
        `
    import { NativeStore, bindingId } from '@agentkit/native';
    if (bindingId !== 'agentkit-native') throw new Error('unexpected binding identity');
    try { await NativeStore.inMemory(); throw new Error('missing addon unexpectedly loaded'); }
    catch (error) { if (!String(error).includes('the native binding failed to load')) throw error; }
  `,
      ],
      { cwd: consumerDirectory, env: toolchainFreeEnvironment },
    );
    process.stdout.write(
      `check-native-artifact: ${manifest.triple} verified structurally (digests, archive layout, publishConfig, loader lockstep, binary container, os/cpu gating, loud missing-addon failure); NOT executed — build-verified only.\n`,
    );
  } else {
    if (!addonInstalled)
      fail(
        `matching optional platform addon was not installed for ${manifest.triple}`,
      );
    for (const [label, runtime] of [
      ["bun", bunBinary],
      ["node", nodeBinary],
    ]) {
      const dbPath = join(consumerDirectory, `acceptance-${label}.db`);
      process.stdout.write(
        `check-native-artifact: acceptance lifecycle under ${label} for ${manifest.triple}…\n`,
      );
      run(runtime, ["consumer.mjs", "run", dbPath], {
        cwd: consumerDirectory,
        env: toolchainFreeEnvironment,
        timeout: 120_000,
      });
      run(runtime, ["consumer.mjs", "reopen", dbPath], {
        cwd: consumerDirectory,
        env: toolchainFreeEnvironment,
        timeout: 120_000,
      });
    }
    rmSync(installedPlatformDir, { recursive: true, force: true });
    run(
      nodeBinary,
      [
        "--input-type=module",
        "-e",
        `
    import { NativeStore } from '@agentkit/native';
    try { await NativeStore.inMemory(); throw new Error('missing addon unexpectedly loaded'); }
    catch (error) { if (!String(error).includes('the native binding failed to load')) throw error; }
  `,
      ],
      { cwd: consumerDirectory, env: toolchainFreeEnvironment },
    );
    process.stdout.write(
      `check-native-artifact: ${manifest.triple} artifact installed isolated and ran the full acceptance lifecycle under bun and node with no reachable toolchain.\n`,
    );
  }
} finally {
  rmSync(temporary, { recursive: true, force: true });
  rmSync(consumerDirectory, { recursive: true, force: true });
}
