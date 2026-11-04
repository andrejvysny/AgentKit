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

/** Extract one member of a packed tarball as raw bytes via the system
 * tar (present on all three runner images; `-O` keeps it on stdout). */
function tarMember(tarball, member) {
  const result = spawnSync("tar", ["-xzf", tarball, "-O", member], {
    maxBuffer: 128 * 1024 * 1024,
  });
  if (result.status !== 0)
    fail(
      `tar could not extract ${member} from ${tarball}: ${result.stderr?.toString().trim() || `status ${result.status}`}`,
    );
  return result.stdout;
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
 * against the triple it claims to be. Never trust a filename. The bytes
 * variant lets the publish gate check the code INSIDE the packed tarball
 * without a temp extraction. */
export function assertBinaryBytesMatchTarget(bytes, target, label) {
  if (target.os === "linux") {
    if (bytes.length < 0x14)
      fail(`${label} is too small to be an ELF (${bytes.length}B)`);
    if (bytes.subarray(0, 4).toString("hex") !== "7f454c46")
      fail(`${label} is not an ELF shared object (target ${target.triple})`);
    const machine = bytes.readUInt16LE(0x12);
    const want = target.cpu === "x64" ? 0x3e : 0xb7; // EM_X86_64 / EM_AARCH64
    if (machine !== want)
      fail(
        `${label} is ELF e_machine 0x${machine.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else if (target.os === "darwin") {
    if (bytes.length < 8)
      fail(`${label} is too small to be a Mach-O (${bytes.length}B)`);
    const magic = bytes.readUInt32LE(0);
    if (magic !== 0xfeedfacf)
      fail(`${label} is not a 64-bit Mach-O (target ${target.triple})`);
    const cputype = bytes.readUInt32LE(4);
    const want = target.cpu === "x64" ? 0x01000007 : 0x0100000c;
    if (cputype !== want)
      fail(
        `${label} is Mach-O cputype 0x${cputype.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else if (target.os === "win32") {
    if (bytes.length < 0x40)
      fail(`${label} is too small to be a PE image (${bytes.length}B)`);
    if (bytes.subarray(0, 2).toString("ascii") !== "MZ")
      fail(`${label} is not a PE image (target ${target.triple})`);
    const peOffset = bytes.readUInt32LE(0x3c);
    if (
      !Number.isSafeInteger(peOffset) ||
      peOffset < 0x40 ||
      peOffset + 6 > bytes.length
    )
      fail(
        `${label} has a corrupt PE header pointer 0x${peOffset.toString(16)}`,
      );
    if (bytes.readUInt32LE(peOffset) !== 0x00004550)
      fail(`${label} lacks the PE\\0\\0 signature at its header pointer`);
    const machine = bytes.readUInt16LE(peOffset + 4);
    const want = target.cpu === "x64" ? 0x8664 : 0xaa64; // IMAGE_FILE_MACHINE_AMD64 / ARM64
    if (machine !== want)
      fail(
        `${label} is PE machine 0x${machine.toString(16)}, expected 0x${want.toString(16)} for ${target.triple}`,
      );
  } else fail(`unknown target os ${target.os}`);
}

export function assertBinaryMatchesTarget(file, target) {
  assertBinaryBytesMatchTarget(readFileSync(file), target, file);
}

/**
 * The manifest is PROOF, not a claim: it is only written once the packed
 * bytes themselves pass every check that does not need a second machine.
 * A manifest whose tarballs would fail downstream verification must never
 * exist on disk. Exported so the checks are testable without a cargo
 * build: `provenanceVersion` is the snapshot's frozen version, or null in
 * a dev checkout (where the dev version may legitimately float).
 */
export function assertPackedArtifacts(
  { wrapperTarball, platformTarball, binaryName },
  target,
  { wrapperVersion, platformVersion, verifiedTypesPath, provenanceVersion },
) {
  // 1. The addon INSIDE the platform tarball must be a binary of the
  //    claimed triple — never a host build smuggled in through some
  //    layout the package.json does not name.
  assertBinaryBytesMatchTarget(
    tarMember(platformTarball, `package/${binaryName}`),
    target,
    `${platformTarball}:package/${binaryName}`,
  );
  // 2. The wrapper must publish EXACTLY the declarations the public-API
  //    gate froze — byte-equal to the verified in-tree rollup, so a
  //    per-job bundling difference can never ship.
  const packedTypes = tarMember(wrapperTarball, "package/dist/index.d.ts");
  const verifiedTypes = readFileSync(
    verifiedTypesPath ??
      join(repoRoot, "packages/native", "dist", "index.d.ts"),
  );
  if (Buffer.compare(packedTypes, verifiedTypes) !== 0)
    fail(
      "packed dist/index.d.ts is not byte-identical to the freshly built " +
        "in-tree declarations — the shipped types would differ from what " +
        "check:api verified",
    );
  // 3. Lockstep versions and semver shape (prerelease tags allowed: the
  //    dev checkout builds `0.1.0-dev` through this same lane; the plain
  //    release shape is enforced at publish by native-publish.mjs);
  //    inside a snapshot, agreement with the frozen provenance version
  //    too (no placeholder version sneaking past a lockstep bug).
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(wrapperVersion ?? ""))
    fail(`wrapper version ${wrapperVersion} is not a semver version`);
  if (platformVersion !== wrapperVersion)
    fail(
      `platform package is at ${platformVersion}, wrapper at ${wrapperVersion} — release must be lockstep`,
    );
  // A snapshot provenance file MUST freeze the version (null = no file,
  // i.e. dev checkout; a file without the field is corruption).
  if (provenanceVersion !== null) {
    if (provenanceVersion !== wrapperVersion)
      fail(
        `snapshot-provenance.json freezes version ${String(provenanceVersion)} but packages carry ${wrapperVersion}`,
      );
  }
}

export function buildNativeReleaseArtifact(target, outDir, options = {}) {
  if (target.cross && !options.allowCross)
    fail(
      `target ${target.triple} is a cross-compilation for this host; pass --allow-cross when the CI job provisions the required toolchain`,
    );
  const rustc = run("rustc", ["--version"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  }).trim();
  if (!rustc)
    fail("rustc --version produced no output for the provenance record");
  // Wrapper TypeScript build via the SAME root entry every other gate
  // uses: the declaration rollup resolves workspace dependencies in
  // dependency order, and a standalone per-package build produces a
  // differently-bundled dist/index.d.ts that the frozen public-API gate
  // never verified. The release must ship exactly what was checked.
  run("bun", ["run", "build"], { cwd: repoRoot });
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

  // The release lane never writes into the working tree: the freshly
  // built addon is staged directly from cargo's target directory.
  const staging = join(outDir, `stage-${target.key}`);
  const staged = stageNativePlatform(repoRoot, staging, target.triple, {
    binaryFrom: built,
  });
  if (staged.binaryName !== binaryName)
    fail(
      `staging named the addon ${staged.binaryName}, expected ${binaryName}`,
    );

  // The wrapper packs IN THE TREE (root or snapshot copy), exactly like
  // the packed-package gate: its prepack rollup re-validates every emitted
  // declaration against the workspace on disk (../../packages/*), which a
  // pruned staging copy cannot host. The platform package packs from
  // staging with its single addon. npm install never runs either pack.
  const tarballs = join(outDir, "tarballs");
  mkdirSync(tarballs, { recursive: true });
  const pack = (dir) =>
    run("npm", ["pack", "--pack-destination", tarballs], { cwd: dir });
  pack(join(repoRoot, "packages", "native"));
  pack(staged.destination);

  const wrapperManifest = JSON.parse(
    readFileSync(join(repoRoot, "packages", "native", "package.json"), "utf8"),
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

  assertPackedArtifacts(
    { wrapperTarball, platformTarball, binaryName },
    target,
    {
      wrapperVersion: wrapperManifest.version,
      platformVersion: platformManifest.version,
      provenanceVersion: existsSync(provenanceFile)
        ? JSON.parse(readFileSync(provenanceFile, "utf8")).version
        : null,
    },
  );

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
      cargoLockSha256: sha256(join(repoRoot, "rust", "Cargo.lock")),
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
