import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  appendFileSync,
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";
import { verifyArtifacts } from "../release/native-publish.mjs";

const VERSION = "9.9.9";
const COMMIT = "0123456789abcdef0123456789abcdef01234567";

const directories: string[] = [];
const sha256 = (file: string) =>
  createHash("sha256").update(readFileSync(file)).digest("hex");

function pack(dir: string, destination: string): string {
  const result = spawnSync("npm", ["pack", "--pack-destination", destination], {
    cwd: dir,
    encoding: "utf8",
  });
  if (result.status !== 0) throw new Error(result.stderr || "npm pack failed");
  const name = result.stdout.trim().split("\n").pop()!;
  return join(destination, name);
}

/** A complete, internally consistent artifact set: five self-consistent
 * manifests + the shared wrapper tarball + the umbrella tarball. */
function artifactSet(base: string): string {
  const dir = join(base, "artifacts");
  mkdirSync(dir, { recursive: true });
  const src = join(base, "src");
  for (const target of NATIVE_PLATFORM_TARGETS) {
    const platformDir = join(src, target.package);
    mkdirSync(platformDir, { recursive: true });
    writeFileSync(
      join(platformDir, "package.json"),
      JSON.stringify(
        {
          name: `@agentkit/native-${target.key}`,
          version: VERSION,
          main: `agentkit-native.${target.key}.node`,
          os: [target.os],
          cpu: [target.cpu],
          publishConfig: { access: "public" },
          ...(target.libc ? { libc: [target.libc] } : {}),
        },
        null,
        2,
      ),
    );
    // Stand-in for the compiled addon: the publish verifier checks digests
    // and manifest identity, not binary containers (that gate lives in
    // check-native-artifact.mjs).
    writeFileSync(
      join(platformDir, `agentkit-native.${target.key}.node`),
      "fake-addon",
    );
  }
  const wrapperDir = join(src, "native");
  mkdirSync(wrapperDir, { recursive: true });
  writeFileSync(
    join(wrapperDir, "package.json"),
    JSON.stringify(
      {
        name: "@agentkit/native",
        version: VERSION,
        main: "./dist/index.js",
        publishConfig: { access: "public" },
      },
      null,
      2,
    ),
  );
  mkdirSync(join(wrapperDir, "dist"), { recursive: true });
  writeFileSync(join(wrapperDir, "dist", "index.js"), "export {};");

  const wrapper = pack(wrapperDir, dir);
  for (const target of NATIVE_PLATFORM_TARGETS) {
    const platform = pack(join(src, target.package), dir);
    writeFileSync(
      join(dir, `artifact-manifest.${target.key}.json`),
      JSON.stringify(
        {
          format: "agentkit-native-artifact-v1",
          triple: target.triple,
          key: target.key,
          package: `@agentkit/native-${target.key}`,
          wrapperPackage: "@agentkit/native",
          version: VERSION,
          structuralOnly: target.cross,
          builtFrom: {
            rustc: "rustc-test",
            triple: target.triple,
            host: "test",
            commit: COMMIT,
          },
          files: {
            wrapper: {
              path: wrapper.slice(dir.length + 1),
              sha256: sha256(wrapper),
              bytes: readFileSync(wrapper).byteLength,
            },
            platform: {
              path: platform.slice(dir.length + 1),
              sha256: sha256(platform),
              bytes: readFileSync(platform).byteLength,
              binary: `agentkit-native.${target.key}.node`,
            },
          },
        },
        null,
        2,
      ),
    );
  }
  const umbrellaDir = join(src, "agentkit");
  mkdirSync(umbrellaDir, { recursive: true });
  writeFileSync(
    join(umbrellaDir, "package.json"),
    JSON.stringify(
      {
        name: "agentkit",
        version: VERSION,
        optionalDependencies: Object.fromEntries(
          NATIVE_PLATFORM_TARGETS.map((t) => [
            `@agentkit/native-${t.key}`,
            VERSION,
          ]),
        ),
      },
      null,
      2,
    ),
  );
  pack(umbrellaDir, dir);
  return dir;
}

let scratch: string;
beforeAll(() => {
  scratch = mkdtempSync(join(tmpdir(), "agentkit-native-publish-"));
});
afterAll(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function fresh(): string {
  const base = mkdtempSync(scratch + "/set-");
  directories.push(base);
  return artifactSet(base);
}

describe("native publication artifact verification", () => {
  it("accepts a complete, digest-consistent artifact set", () => {
    const plan = verifyArtifacts(fresh(), VERSION, COMMIT);
    expect(plan.platforms.map((p: { key: string }) => p.key).sort()).toEqual(
      NATIVE_PLATFORM_TARGETS.map((t) => t.key).sort(),
    );
    expect(plan.wrapper).toContain("agentkit-native-9.9.9.tgz");
    expect(plan.umbrella).toContain("agentkit-9.9.9.tgz");
  });

  it("refuses a missing triple", () => {
    const dir = fresh();
    rmSync(join(dir, "artifact-manifest.darwin-x64.json"));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "missing artifact manifest",
    );
  });

  it("refuses tampered platform bytes", () => {
    const dir = fresh();
    const platform = JSON.parse(
      readFileSync(join(dir, "artifact-manifest.linux-x64-gnu.json"), "utf8"),
    );
    appendFileSync(join(dir, platform.files.platform.path), "tampered");
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "sha256 mismatch",
    );
  });

  it("refuses a wrapper tarball that differs across build jobs", () => {
    const dir = fresh();
    // Re-pack a second, DIFFERENT wrapper and point one manifest at it
    // while leaving its (self-consistent) digest intact: the cross-job
    // identity check must catch the nondeterminism.
    const otherWrapperDir = join(dir, "other-wrapper");
    mkdirSync(join(otherWrapperDir, "dist"), { recursive: true });
    writeFileSync(
      join(otherWrapperDir, "package.json"),
      JSON.stringify({
        name: "@agentkit/native",
        version: VERSION,
        main: "./dist/index.js",
        publishConfig: { access: "public" },
      }),
    );
    writeFileSync(join(otherWrapperDir, "dist", "index.js"), "export {}; //v2");
    const otherPackDir = join(dir, "other-pack");
    mkdirSync(otherPackDir, { recursive: true });
    const other = pack(otherWrapperDir, otherPackDir);
    const manifestPath = join(dir, "artifact-manifest.darwin-arm64.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.wrapper = {
      path: `${otherPackDir.slice(dir.length + 1)}/${other.slice(otherPackDir.length + 1)}`,
      sha256: sha256(other),
      bytes: readFileSync(other).byteLength,
    };
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "not byte-identical",
    );
  });

  it("refuses version drift in a platform artifact", () => {
    const dir = fresh();
    const manifestPath = join(dir, "artifact-manifest.win32-x64-msvc.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.version = "9.9.8";
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "release is 9.9.9",
    );
  });

  it("refuses a build from a different commit", () => {
    const dir = fresh();
    const manifestPath = join(dir, "artifact-manifest.darwin-arm64.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.builtFrom.commit = "f".repeat(40);
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "not the release commit",
    );
  });

  it("refuses an umbrella that under-pins the platform packages", () => {
    const dir = fresh();
    const tarball = join(dir, `agentkit-${VERSION}.tgz`);
    const repackDir = join(dir, "umbrella-drift");
    mkdirSync(repackDir, { recursive: true });
    writeFileSync(
      join(repackDir, "package.json"),
      JSON.stringify({
        name: "agentkit",
        version: VERSION,
        optionalDependencies: Object.fromEntries(
          NATIVE_PLATFORM_TARGETS.map((t) => [
            `@agentkit/native-${t.key}`,
            "9.9.8",
          ]),
        ),
      }),
    );
    const driftPackDir = join(dir, "umbrella-drift-pack");
    mkdirSync(driftPackDir, { recursive: true });
    const drifted = pack(repackDir, driftPackDir);
    rmSync(tarball);
    cpSync(drifted, tarball);
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "does not pin @agentkit/native",
    );
  });

  it("refuses a still-private platform tarball", () => {
    const dir = fresh();
    const platformDir = join(dir, "private-platform");
    mkdirSync(platformDir, { recursive: true });
    writeFileSync(
      join(platformDir, "package.json"),
      JSON.stringify({
        name: "@agentkit/native-linux-arm64-gnu",
        version: VERSION,
        private: true,
        main: "agentkit-native.linux-arm64-gnu.node",
      }),
    );
    writeFileSync(
      join(platformDir, "agentkit-native.linux-arm64-gnu.node"),
      "fake",
    );
    const privatePackDir = join(dir, "private-pack");
    mkdirSync(privatePackDir, { recursive: true });
    const tarball = pack(platformDir, privatePackDir);
    const manifestPath = join(dir, "artifact-manifest.linux-arm64-gnu.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.platform = {
      path: `private-pack/${tarball.slice(privatePackDir.length + 1)}`,
      sha256: sha256(tarball),
      bytes: readFileSync(tarball).byteLength,
      binary: "agentkit-native.linux-arm64-gnu.node",
    };
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "still private",
    );
  });
});
