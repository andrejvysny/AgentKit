import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  appendFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";
import {
  compareSemver,
  publishArtifacts,
  resolveDistTag,
  verifyArtifacts,
} from "../release/native-publish.mjs";

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

/** Minimal honest container for a triple: the publish verifier reads the
 * machine identity out of the packed bytes, so fixtures must carry the
 * same headers a compiled addon would. */
function containerBytes(target: {
  os: string;
  cpu: string;
  triple: string;
}): Buffer {
  if (target.os === "linux") {
    const b = Buffer.alloc(64);
    b.write("ELF", 0, "latin1");
    b.writeUInt16LE(target.cpu === "x64" ? 0x3e : 0xb7, 0x12);
    return b;
  }
  if (target.os === "darwin") {
    const b = Buffer.alloc(64);
    b.writeUInt32LE(0xfeedfacf, 0);
    b.writeUInt32LE(target.cpu === "x64" ? 0x01000007 : 0x0100000c, 4);
    return b;
  }
  const b = Buffer.alloc(96);
  b.write("MZ", 0, "ascii");
  b.writeUInt32LE(64, 0x3c); // e_lfanew
  b.write("PE\0\0", 64, "ascii");
  b.writeUInt16LE(target.cpu === "x64" ? 0x8664 : 0xaa64, 68);
  return b;
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
    writeFileSync(
      join(platformDir, `agentkit-native.${target.key}.node`),
      containerBytes(target),
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
            cargoLockSha256: "a".repeat(64),
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
  const base = mkdtempSync(`${scratch}/set-`);
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
      "members are not identical",
    );
  });

  it("accepts wrapper tarballs with the same members but different gzip bytes", () => {
    const dir = fresh();
    const wrapper = join(dir, `agentkit-native-${VERSION}.tgz`);
    // Re-gzip the same tar payload with a different header (mtime + name
    // field, level) the way another runner OS would: raw bytes differ,
    // members do not. Point one manifest at it.
    const tar = Bun.gunzipSync(readFileSync(wrapper));
    const regzipped = Buffer.from(Bun.gzipSync(tar, { level: 1 }));
    regzipped.writeUInt32LE(0x5f5e100, 4); // gzip MTIME field
    const altDir = join(dir, "alt-wrapper");
    mkdirSync(altDir, { recursive: true });
    const alt = join(altDir, `agentkit-native-${VERSION}.tgz`);
    writeFileSync(alt, regzipped);
    expect(sha256(alt)).not.toBe(sha256(wrapper));
    const manifestPath = join(dir, "artifact-manifest.win32-x64-msvc.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.wrapper = {
      path: `alt-wrapper/agentkit-native-${VERSION}.tgz`,
      sha256: sha256(alt),
      bytes: regzipped.byteLength,
    };
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).not.toThrow();
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

  it("refuses a foreign-machine binary packed inside the right tarball", () => {
    const dir = fresh();
    // Self-consistent repack: correct digests everywhere, but the linux
    // addon's bytes are a Mach-O. Only the in-tarball machine check can
    // catch what a manifest-only verifier would publish.
    const platformDir = join(dir, "foreign-platform");
    mkdirSync(platformDir, { recursive: true });
    const target = NATIVE_PLATFORM_TARGETS.find(
      (t) => t.key === "linux-x64-gnu",
    )!;
    writeFileSync(
      join(platformDir, "package.json"),
      JSON.stringify({
        name: `@agentkit/native-${target.key}`,
        version: VERSION,
        main: `agentkit-native.${target.key}.node`,
        os: [target.os],
        cpu: [target.cpu],
        libc: ["glibc"],
        publishConfig: { access: "public" },
      }),
    );
    writeFileSync(
      join(platformDir, `agentkit-native.${target.key}.node`),
      containerBytes({ os: "darwin", cpu: "x64", triple: "x" }),
    );
    const packDir = join(dir, "foreign-pack");
    mkdirSync(packDir, { recursive: true });
    const tarball = pack(platformDir, packDir);
    const manifestPath = join(dir, "artifact-manifest.linux-x64-gnu.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.platform = {
      path: `foreign-pack/${tarball.slice(packDir.length + 1)}`,
      sha256: sha256(tarball),
      bytes: readFileSync(tarball).byteLength,
      binary: `agentkit-native.${target.key}.node`,
    };
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "not an ELF shared object",
    );
  });

  it("refuses a manifest path that escapes the artifact directory", () => {
    const dir = fresh();
    const manifestPath = join(dir, "artifact-manifest.darwin-arm64.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.platform.path = "../../outside.tgz";
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "not the expected",
    );
  });

  it("refuses build jobs that disagree on Cargo.lock", () => {
    const dir = fresh();
    const manifestPath = join(dir, "artifact-manifest.darwin-x64.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.builtFrom.cargoLockSha256 = "b".repeat(64);
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "different Cargo.lock",
    );
  });

  it("refuses a platform tarball packed under the wrong package name", () => {
    const dir = fresh();
    const target = NATIVE_PLATFORM_TARGETS.find(
      (t) => t.key === "win32-x64-msvc",
    )!;
    // Hand-built tarball so the FILE name matches the expected win32
    // artifact while the packed package.json claims a different triple's
    // package (npm pack would name the file after the claimed name and
    // the basename gate would fire first).
    const stage = join(dir, "renamed-stage");
    mkdirSync(join(stage, "package"), { recursive: true });
    writeFileSync(
      join(stage, "package", "package.json"),
      JSON.stringify({
        name: "@agentkit/native-darwin-arm64",
        version: VERSION,
        main: `agentkit-native.${target.key}.node`,
        os: [target.os],
        cpu: [target.cpu],
        publishConfig: { access: "public" },
      }),
    );
    writeFileSync(
      join(stage, "package", `agentkit-native.${target.key}.node`),
      containerBytes(target),
    );
    const tarball = join(dir, `agentkit-native-${target.key}-${VERSION}.tgz`);
    const packed = spawnSync("tar", ["-czf", tarball, "-C", stage, "package"], {
      encoding: "utf8",
    });
    if (packed.status !== 0) throw new Error(`tar failed: ${packed.stderr}`);
    const manifestPath = join(dir, "artifact-manifest.win32-x64-msvc.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.files.platform = {
      path: `agentkit-native-${target.key}-${VERSION}.tgz`,
      sha256: sha256(tarball),
      bytes: readFileSync(tarball).byteLength,
      binary: `agentkit-native.${target.key}.node`,
    };
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "is package @agentkit/native-darwin-arm64",
    );
  });

  it("refuses a wrapper that under-pins its own platform builds", () => {
    const dir = fresh();
    // Re-pack the wrapper WITHOUT one platform pin and update every
    // manifest to its (consistent) digests: cross-job identity still
    // holds, so only the packed-pin check can refuse it.
    const wrapperDir = join(dir, "drift-wrapper");
    mkdirSync(join(wrapperDir, "dist"), { recursive: true });
    const pins = Object.fromEntries(
      NATIVE_PLATFORM_TARGETS.filter((t) => t.key !== "darwin-x64").map((t) => [
        `@agentkit/native-${t.key}`,
        VERSION,
      ]),
    );
    writeFileSync(
      join(wrapperDir, "package.json"),
      JSON.stringify({
        name: "@agentkit/native",
        version: VERSION,
        main: "./dist/index.js",
        publishConfig: { access: "public" },
        optionalDependencies: pins,
      }),
    );
    writeFileSync(join(wrapperDir, "dist", "index.js"), "export {};");
    const packDir = join(dir, "drift-pack");
    mkdirSync(packDir, { recursive: true });
    const drifted = pack(wrapperDir, packDir);
    for (const target of NATIVE_PLATFORM_TARGETS) {
      const manifestPath = join(dir, `artifact-manifest.${target.key}.json`);
      const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
      manifest.files.wrapper = {
        path: `drift-pack/${drifted.slice(packDir.length + 1)}`,
        sha256: sha256(drifted),
        bytes: readFileSync(drifted).byteLength,
      };
      writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
    }
    expect(() => verifyArtifacts(dir, VERSION, COMMIT)).toThrow(
      "does not pin @agentkit/native-darwin-x64",
    );
  });
});

describe("native publication resume semantics", () => {
  let fakeBin: string;

  beforeAll(() => {
    fakeBin = mkdtempSync(join(tmpdir(), "agentkit-fake-npm-"));
    writeFileSync(
      join(fakeBin, "npm.js"),
      `const fs = require("node:fs");
const [,, cmd, target, third] = process.argv;
if (cmd === "view" && third === "dist-tags.latest") {
  const latest = JSON.parse(process.env.FAKE_LATEST ?? "{}")[target];
  if (latest === undefined) { console.error("npm ERR! code E404"); process.exit(1); }
  console.log(latest); process.exit(0);
}
if (cmd === "view") {
  const registry = JSON.parse(process.env.FAKE_REGISTRY ?? "{}");
  const integrity = registry[target];
  if (integrity === undefined) { console.error("npm ERR! code E404"); process.exit(1); }
  console.log(integrity); process.exit(0);
}
if (cmd === "publish") {
  fs.appendFileSync(process.env.PUBLISH_MARKER ?? "/dev/null", process.argv[3] + "\\n");
  fs.appendFileSync(process.env.PUBLISH_ARGS ?? "/dev/null", process.argv.slice(2).join(" ") + "\\n");
  process.exit(0);
}
process.exit(2);
`,
    );
    writeFileSync(
      join(fakeBin, "npm"),
      '#!/bin/sh\nexec node "$(dirname "$0")/npm.js" "$@"\n',
      { mode: 0o755 },
    );
  });

  afterAll(() => {
    rmSync(fakeBin, { recursive: true, force: true });
  });

  function withFakeNpm<T>(
    dir: string,
    registry: Record<string, string>,
    run: () => T,
    latest: Record<string, string> = {},
  ): { result: T; published: string[]; publishArgs: string[] } {
    const marker = join(dir, "published.log");
    const argsLog = join(dir, "publish-args.log");
    const lines = (file: string) =>
      existsSync(file)
        ? readFileSync(file, "utf8").trim().split("\n").filter(Boolean)
        : [];
    const previousToken = process.env.NPM_TOKEN;
    const previousUserconfig = process.env.NPM_CONFIG_USERCONFIG;
    process.env.NPM_TOKEN = "fake-token";
    process.env.FAKE_REGISTRY = JSON.stringify(registry);
    process.env.FAKE_LATEST = JSON.stringify(latest);
    process.env.PUBLISH_MARKER = marker;
    process.env.PUBLISH_ARGS = argsLog;
    try {
      const result = run();
      return {
        result,
        published: lines(marker),
        publishArgs: lines(argsLog),
      };
    } finally {
      if (previousUserconfig === undefined)
        delete process.env.NPM_CONFIG_USERCONFIG;
      else process.env.NPM_CONFIG_USERCONFIG = previousUserconfig;
      if (previousToken === undefined) delete process.env.NPM_TOKEN;
      else process.env.NPM_TOKEN = previousToken;
      delete process.env.FAKE_REGISTRY;
      delete process.env.FAKE_LATEST;
      delete process.env.PUBLISH_MARKER;
      delete process.env.PUBLISH_ARGS;
    }
  }

  const publishOptions = () => ({
    npm: join(fakeBin, "npm"),
    allowUnexecutedTargets: true,
  });

  it("skips only byte-identical registry content and publishes the rest", () => {
    const dir = fresh();
    const already = join(dir, "agentkit-native-linux-x64-gnu-9.9.9.tgz");
    const integrity = `sha512-${createHash("sha512").update(readFileSync(already)).digest("base64")}`;
    const { published } = withFakeNpm(
      dir,
      { "@agentkit/native-linux-x64-gnu@9.9.9": integrity },
      () => publishArtifacts(dir, VERSION, COMMIT, true, publishOptions()),
    );
    const names = published.map((p) => p.split("/").pop());
    // A partial run resumes: the identical package is skipped, everything
    // else still goes out, platforms before the wrapper before the umbrella.
    expect(names).not.toContain("agentkit-native-linux-x64-gnu-9.9.9.tgz");
    expect(names).toContain("agentkit-native-darwin-x64-9.9.9.tgz");
    expect(names).toContain("agentkit-native-9.9.9.tgz");
    expect(names).toContain("agentkit-9.9.9.tgz");
    expect(names.indexOf("agentkit-native-9.9.9.tgz")).toBeGreaterThan(
      names.indexOf("agentkit-native-win32-x64-msvc-9.9.9.tgz"),
    );
    expect(names.indexOf("agentkit-9.9.9.tgz")).toBe(names.length - 1);
  });

  it("refuses to skip when the registry holds different bytes for the version", () => {
    const dir = fresh();
    expect(() =>
      withFakeNpm(
        dir,
        {
          "@agentkit/native-linux-x64-gnu@9.9.9":
            "sha512-WRONGWRONGWRONGWRONGWRONGWRONGWRONGWRONGWRONGWRONGWRONG=",
        },
        () => publishArtifacts(dir, VERSION, COMMIT, true, publishOptions()),
      ),
    ).toThrow("different bytes");
  });

  it("publishes a plain release under --tag latest", () => {
    const dir = fresh();
    const { publishArgs } = withFakeNpm(
      dir,
      {},
      () => publishArtifacts(dir, VERSION, COMMIT, true, publishOptions()),
      { agentkit: "9.9.8" },
    );
    expect(publishArgs.length).toBe(NATIVE_PLATFORM_TARGETS.length + 2);
    for (const line of publishArgs) expect(line).toContain("--tag latest");
  });

  it("refuses to move latest backwards", () => {
    const dir = fresh();
    const { publishArgs } = withFakeNpm(
      dir,
      {},
      () => {
        expect(() =>
          publishArtifacts(dir, VERSION, COMMIT, true, publishOptions()),
        ).toThrow("refusing to move latest backwards");
      },
      { "@agentkit/native": "10.0.0" },
    );
    // Preflight: nothing was published before the refusal.
    expect(publishArgs).toEqual([]);
  });

  it("allows an explicit next tag even when latest is newer", () => {
    const dir = fresh();
    const { publishArgs } = withFakeNpm(
      dir,
      {},
      () =>
        publishArtifacts(dir, VERSION, COMMIT, true, {
          ...publishOptions(),
          tag: "next",
        }),
      { "@agentkit/native": "10.0.0" },
    );
    for (const line of publishArgs) expect(line).toContain("--tag next");
  });

  it("refuses structural-only targets unless explicitly allowed", () => {
    const dir = fresh();
    expect(() =>
      withFakeNpm(dir, {}, () =>
        publishArtifacts(dir, VERSION, COMMIT, true, {
          npm: join(fakeBin, "npm"),
        }),
      ),
    ).toThrow("structural-only");
    const allowed = fresh();
    const { published } = withFakeNpm(allowed, {}, () =>
      publishArtifacts(allowed, VERSION, COMMIT, true, publishOptions()),
    );
    expect(published.length).toBe(NATIVE_PLATFORM_TARGETS.length + 2);
  });
});

describe("npm dist-tag resolution and semver order", () => {
  it("resolves auto to next for a prerelease and latest otherwise", () => {
    expect(resolveDistTag("1.2.3")).toBe("latest");
    expect(resolveDistTag("1.2.3-rc.1", "auto")).toBe("next");
    expect(resolveDistTag("1.2.3", "next")).toBe("next");
    expect(() => resolveDistTag("1.2.3", "1.2.3")).toThrow("dist-tag");
  });

  it("orders versions with prerelease below release", () => {
    expect(compareSemver("10.0.0", "9.9.9")).toBe(1);
    expect(compareSemver("1.2.3-rc.1", "1.2.3")).toBe(-1);
    expect(compareSemver("1.2.3-rc.2", "1.2.3-rc.10")).toBe(-1);
    expect(compareSemver("1.2.3", "1.2.3")).toBe(0);
  });
});
