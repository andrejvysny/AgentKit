/**
 * Gate tests for the native release builder
 * (`rust/scripts/build-native-release-artifact.mjs`). The full build needs
 * a cargo toolchain per triple, so these pin the two things the release
 * story actually depends on and that must never regress:
 *
 *   1. `assertPackedArtifacts` — the manifest-is-proof gate. It runs on
 *      real tarballs built here from honest fixtures: a wrong-machine
 *      addon, drifted published types, broken lockstep, a placeholder
 *      version, or a provenance disagreement must each refuse the
 *      manifest loudly.
 *   2. The CLI guards that must fire BEFORE any expensive work: usage,
 *      out-of-repo `--out`, unknown triple, and the cross-compilation
 *      guard (a misrouted matrix row may not relabel a host build).
 */
import { afterAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { assertPackedArtifacts } from "../../rust/scripts/build-native-release-artifact.mjs";
import { nativeTargetByTriple } from "../../rust/scripts/native-platform-targets.mjs";

const repoRoot = join(import.meta.dir, "..", "..");
const builder = join(
  repoRoot,
  "rust",
  "scripts",
  "build-native-release-artifact.mjs",
);
const linuxX64 = nativeTargetByTriple("x86_64-unknown-linux-gnu");

const directories: string[] = [];
afterAll(() => {
  for (const dir of directories) rmSync(dir, { recursive: true, force: true });
});

function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  directories.push(dir);
  return dir;
}

/** ELF64 x86_64 / aarch64 header good enough for the container check. */
function fakeElf(cpu: "x64" | "arm64"): Buffer {
  const bytes = Buffer.alloc(64);
  bytes.set([0x7f, 0x45, 0x4c, 0x46], 0);
  bytes.writeUInt16LE(cpu === "x64" ? 0x3e : 0xb7, 0x12);
  return bytes;
}

function tar(dir: string, out: string): string {
  const result = spawnSync("tar", ["-czf", out, "-C", dir, "package"], {
    encoding: "utf8",
  });
  if (result.status !== 0) throw new Error(result.stderr || "tar failed");
  return out;
}

/** Pack a platform-shaped tarball (addon at the package root, as the
 * platform package `main` names) and a wrapper-shaped tarball carrying the
 * published declarations. */
function packedPair(
  options: {
    addon?: Buffer;
    binaryName?: string;
    publishedTypes?: string;
    verifiedTypes?: string;
  } = {},
) {
  const root = tempDir("agentkit-packed-");
  const binaryName = options.binaryName ?? "agentkit-native.linux-x64-gnu.node";
  const platformDir = join(root, "platform", "package");
  mkdirSync(platformDir, { recursive: true });
  writeFileSync(join(platformDir, binaryName), options.addon ?? fakeElf("x64"));
  writeFileSync(
    join(platformDir, "package.json"),
    JSON.stringify({ name: "@agentkit/native-linux-x64-gnu" }),
  );
  const wrapperDir = join(root, "wrapper", "package", "dist");
  mkdirSync(wrapperDir, { recursive: true });
  const published = options.publishedTypes ?? "export declare const a: 1;\n";
  writeFileSync(join(wrapperDir, "index.d.ts"), published);
  const platformTarball = tar(
    join(root, "platform"),
    join(root, "platform.tgz"),
  );
  const wrapperTarball = tar(join(root, "wrapper"), join(root, "wrapper.tgz"));
  const verifiedTypesPath = join(root, "verified.d.ts");
  writeFileSync(verifiedTypesPath, options.verifiedTypes ?? published);
  return {
    files: { wrapperTarball, platformTarball, binaryName },
    verifiedTypesPath,
  };
}

const baseVersions = {
  wrapperVersion: "1.2.3",
  platformVersion: "1.2.3",
  provenanceVersion: null as string | null,
};

describe("assertPackedArtifacts (manifest-is-proof gate)", () => {
  test("accepts an honest artifact set", () => {
    const { files, verifiedTypesPath } = packedPair();
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        verifiedTypesPath,
      }),
    ).not.toThrow();
  });

  test("refuses a wrong-machine addon inside the platform tarball", () => {
    const { files, verifiedTypesPath } = packedPair({
      addon: fakeElf("arm64"),
    });
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        verifiedTypesPath,
      }),
    ).toThrow(/e_machine 0xb7, expected 0x3e/);
  });

  test("refuses a non-binary addon payload", () => {
    const { files, verifiedTypesPath } = packedPair({
      addon: Buffer.from("not a binary at all, definitely not an ELF"),
    });
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        verifiedTypesPath,
      }),
    ).toThrow(/not an ELF shared object/);
  });

  test("refuses published types that drifted from the verified rollup", () => {
    const { files, verifiedTypesPath } = packedPair({
      publishedTypes: "export declare const a: 2;\n",
      verifiedTypes: "export declare const a: 1;\n",
    });
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        verifiedTypesPath,
      }),
    ).toThrow(/not byte-identical/);
  });

  test("refuses a platform version that drifted from the wrapper", () => {
    const { files, verifiedTypesPath } = packedPair();
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        platformVersion: "1.2.0",
        verifiedTypesPath,
      }),
    ).toThrow(/lockstep/);
  });

  test("refuses non-semver versions", () => {
    for (const version of ["9.9", "v1.2.3", "1.2.3.4", "", "latest"]) {
      const { files, verifiedTypesPath } = packedPair();
      expect(() =>
        assertPackedArtifacts(files, linuxX64, {
          ...baseVersions,
          wrapperVersion: version,
          platformVersion: version,
          provenanceVersion: null,
          verifiedTypesPath,
        }),
      ).toThrow(/not a semver version/);
    }
  });

  test("refuses a snapshot provenance that disagrees with the packages", () => {
    const { files, verifiedTypesPath } = packedPair();
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        provenanceVersion: "9.9.9",
        verifiedTypesPath,
      }),
    ).toThrow(/freezes version 9\.9\.9/);
  });

  test("refuses a provenance file whose version field is missing", () => {
    const { files, verifiedTypesPath } = packedPair();
    expect(() =>
      assertPackedArtifacts(files, linuxX64, {
        ...baseVersions,
        provenanceVersion: undefined,
        verifiedTypesPath,
      }),
    ).toThrow(/freezes version undefined/);
  });

  test("refuses a tarball missing the member it claims", () => {
    const { files, verifiedTypesPath } = packedPair();
    expect(() =>
      assertPackedArtifacts(
        { ...files, binaryName: "agentkit-native.linux-arm64-gnu.node" },
        linuxX64,
        { ...baseVersions, verifiedTypesPath },
      ),
    ).toThrow(/tar could not extract/);
  });
});

describe("builder CLI guards (fire before any build work)", () => {
  const runBuilder = (args: string[]) =>
    spawnSync(process.execPath, [builder, ...args], {
      encoding: "utf8",
      cwd: repoRoot,
    });

  test("usage error without required flags", () => {
    const result = runBuilder([]);
    expect(result.status).not.toBe(0);
    expect(`${result.stderr}${result.stdout}`).toContain("usage:");
  });

  test("unknown triple is refused", () => {
    const result = runBuilder([
      "--target",
      "sparc-unknown-linux",
      "--out",
      "/tmp",
    ]);
    expect(result.status).not.toBe(0);
    expect(`${result.stderr}${result.stdout}`).toMatch(/sparc|unknown|target/i);
  });

  test("--out inside the repository is refused", () => {
    // A real in-repo path must trip the guard (the snapshot lane packs
    // from a copy OUTSIDE the repo; an in-repo out would litter the tree).
    const repoDir = join(repoRoot, ".build-native-out-test");
    mkdirSync(repoDir, { recursive: true });
    try {
      const result = runBuilder([
        "--target",
        linuxX64.triple,
        "--out",
        repoDir,
      ]);
      expect(result.status).not.toBe(0);
      expect(`${result.stderr}${result.stdout}`).toContain(
        "--out must be outside the repository",
      );
    } finally {
      rmSync(repoDir, { recursive: true, force: true });
    }
  });

  test("a cross-compilation row without --allow-cross refuses before building", () => {
    // x86_64-apple-darwin is a cross row; from ANY host the guard must
    // fire before rustc/cargo run, otherwise a misrouted matrix cell
    // could relabel a host build as another triple.
    const result = runBuilder([
      "--target",
      "x86_64-apple-darwin",
      "--out",
      tempDir("agentkit-out-"),
    ]);
    expect(result.status).not.toBe(0);
    expect(`${result.stderr}${result.stdout}`).toContain("cross-compilation");
  });
});
