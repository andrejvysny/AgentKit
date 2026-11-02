#!/usr/bin/env node
/**
 * Build one prebuilt platform artifact for the native npm release.
 *
 *   node rust/scripts/build-native-release-artifact.mjs \
 *     --target <rust-target-triple> --out <existing-outside-repo-dir>
 *
 * Builds `agentkit-napi` as a cdylib for the exact triple (never the host
 * relabelled — the output filename and the packed binary are derived from
 * the triple), verifies the compiled library's container format and machine
 * against the triple, stages it into a copy of the platform package, and
 * `npm pack`s the platform tarball AND the wrapper tarball into `--out`
 * together with `artifact-manifest.json` (sha256 + build provenance).
 *
 * A triple whose os/cpu the current machine cannot execute is packed but
 * marked `structuralOnly` — the release lane must never present a
 * never-executed cell as an executed one (M2.1 A4 rule).
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { nativeTargetByTriple } from "./native-platform-targets.mjs";
import { stageNativePlatform } from "./stage-native-platform.mjs";

const repoRoot = realpathSync(
  resolve(dirname(fileURLToPath(import.meta.url)), "../.."),
);

function fail(message) {
  throw new Error(`build-native-release-artifact: ${message}`);
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    stdio: "inherit",
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(
      `build-native-release-artifact: ${command} ${args.join(" ")} failed with status ${result.status}`,
    );
  return result.stdout ?? "";
}

/** Container-format and machine identity of a compiled addon, checked
 * against the triple it claims to be. Never trust a filename. */
export function assertBinaryMatchesTarget(file, target) {
  const head = readFileSync(file).subarray(0, 64);
  const u16 = (at) => head.readUInt16LE(at);
  const u32 = (at) => head.readUInt32LE(at);
  if (target.os === "linux") {
    if (head.subarray(0, 4).toString("hex") !== "7f454c46")
      fail(`${file} is not an ELF shared object (target ${target.triple})`);
    const machine = u16(0x12);
    const want = target.cpu === "x64" ? 0x3e : 0xb7; // EM_X86_64 / EM_AARCH64
    if (machine !== want)
      fail(
        `${file} is ELF e_machine 0x${machine.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else if (target.os === "darwin") {
    const magic = u32(0);
    if (magic !== 0xfeedfacf)
      fail(`${file} is not a 64-bit Mach-O (target ${target.triple})`);
    const cputype = u32(4);
    const want = target.cpu === "x64" ? 0x01000007 : 0x0100000c;
    if (cputype !== want)
      fail(
        `${file} is Mach-O cputype 0x${cputype.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else if (target.os === "win32") {
    if (head.subarray(0, 2).toString("ascii") !== "MZ")
      fail(`${file} is not a PE image (target ${target.triple})`);
    const peOffset = u32(0x3c);
    const machine = readFileSync(file).readUInt16LE(peOffset + 4);
    const want = target.cpu === "x64" ? 0x8664 : 0xaa64; // IMAGE_FILE_MACHINE_AMD64 / ARM64
    if (machine !== want)
      fail(
        `${file} is PE machine 0x${machine.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else fail(`unknown target os ${target.os}`);
}

export function buildNativeReleaseArtifact(target, outDir, options = {}) {
  if (target.cross && !options.allowCross)
    fail(
      `target ${target.triple} is a cross-compilation for this host; pass --allow-cross when the CI job provisions the required toolchain`,
    );
  const rustc = run("rustc", ["--version"], { encoding: "utf8" }).trim();
  // Wrapper TypeScript build (dist + bundled declarations run at prepack).
  run("bun", ["run", "--cwd", "packages/native", "build"]);
  // The cdylib for the exact triple — same cargo build `napi build --platform`
  // performs for the host, without touching the committed loader files.
  run(
    "cargo",
    [
      "build",
      "--release",
      "--target",
      target.triple,
      "-p",
      "agentkit-napi",
      "--locked",
    ],
    { cwd: join(repoRoot, "rust"), encoding: "utf8" },
  );
  const cargoName = "agentkit_napi";
  const cargoFile =
    target.os === "linux"
      ? `lib${cargoName}.so`
      : target.os === "darwin"
        ? `lib${cargoName}.dylib`
        : `${cargoName}.dll`;
  const built = join(
    repoRoot,
    "rust",
    "target",
    target.triple,
    "release",
    cargoFile,
  );
  if (!existsSync(built)) fail(`cargo produced no ${built}`);
  assertBinaryMatchesTarget(built, target);

  const binaryName = `agentkit-native.${target.key}.node`;
  copyFileSync(
    built,
    join(repoRoot, "packages", "native", "generated", binaryName),
  );

  const staging = join(outDir, `stage-${target.key}`);
  const staged = stageNativePlatform(repoRoot, staging, target.triple);

  // Wrapper staging copy (same shape the packed-package gate uses, so the
  // prepack lifecycle scripts resolve `../../scripts/*`).
  const stagedWrapper = join(staging, "packages", "native");
  mkdirSync(join(staging, "packages"), { recursive: true });
  cpSync(join(repoRoot, "packages", "native"), stagedWrapper, {
    recursive: true,
  });
  cpSync(join(repoRoot, "scripts"), join(staging, "scripts"), {
    recursive: true,
  });

  const tarballs = join(outDir, "tarballs");
  mkdirSync(tarballs, { recursive: true });
  const pack = (dir) =>
    run("npm", ["pack", "--pack-destination", tarballs], { cwd: dir });
  pack(stagedWrapper);
  pack(staged.destination);

  const wrapperManifest = JSON.parse(
    readFileSync(join(stagedWrapper, "package.json"), "utf8"),
  );
  // Source identity: inside a release snapshot the frozen baseline in
  // snapshot-provenance.json is the commit; in a dev checkout, HEAD.
  let commit;
  const provenanceFile = join(repoRoot, "snapshot-provenance.json");
  if (existsSync(provenanceFile))
    commit = JSON.parse(readFileSync(provenanceFile, "utf8")).baseline;
  else {
    try {
      commit =
        spawnSync("git", ["rev-parse", "HEAD"], {
          cwd: repoRoot,
          encoding: "utf8",
        }).stdout?.trim() || undefined;
    } catch {
      commit = undefined;
    }
  }
  if (!commit) fail("cannot determine the source commit for the manifest");
  const platformManifest = JSON.parse(
    readFileSync(join(staged.destination, "package.json"), "utf8"),
  );
  const wrapperTarball = join(
    tarballs,
    `agentkit-native-${wrapperManifest.version}.tgz`,
  );
  const platformTarball = join(
    tarballs,
    `agentkit-native-${target.key}-${platformManifest.version}.tgz`,
  );
  for (const t of [wrapperTarball, platformTarball])
    if (!existsSync(t)) fail(`npm pack produced no ${t}`);

  const structuralOnly =
    options.structuralOnly ??
    (process.platform !== target.os || process.arch !== target.cpu);

  const manifest = {
    format: "agentkit-native-artifact-v1",
    triple: target.triple,
    key: target.key,
    package: platformManifest.name,
    wrapperPackage: wrapperManifest.name,
    version: wrapperManifest.version,
    structuralOnly,
    builtFrom: {
      rustc,
      triple: target.triple,
      host: `${process.platform}-${process.arch}`,
      commit,
    },
    files: {
      wrapper: {
        path: relative(outDir, wrapperTarball),
        sha256: sha256(wrapperTarball),
        bytes: readFileSync(wrapperTarball).byteLength,
      },
      platform: {
        path: relative(outDir, platformTarball),
        sha256: sha256(platformTarball),
        bytes: readFileSync(platformTarball).byteLength,
        binary: binaryName,
      },
    },
  };
  const manifestPath = join(outDir, `artifact-manifest.${target.key}.json`);
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const args = process.argv.slice(2);
  const flag = (name) => {
    const at = args.indexOf(name);
    return at >= 0 ? args[at + 1] : undefined;
  };
  const triple = flag("--target");
  const out = flag("--out");
  if (!triple || !out)
    throw new Error(
      "usage: node rust/scripts/build-native-release-artifact.mjs --target <rust-target-triple> --out <existing-outside-repo-dir> [--allow-cross]",
    );
  const target = nativeTargetByTriple(triple);
  const outDir = resolve(out);
  if (!existsSync(outDir)) fail(`--out must exist: ${outDir}`);
  const inside = relative(repoRoot, outDir);
  if (
    inside === "" ||
    (inside !== ".." && !inside.startsWith(`..${sep}`) && !isAbsolute(inside))
  )
    fail("--out must be outside the repository");
  const manifest = buildNativeReleaseArtifact(target, outDir, {
    allowCross: args.includes("--allow-cross"),
  });
  process.stdout.write(
    `${manifest.structuralOnly ? "packed (structural-only)" : "packed (host-executable)"} ${manifest.package}@${manifest.version} for ${manifest.triple}\n${join(outDir, `artifact-manifest.${target.key}.json`)}\n`,
  );
}
