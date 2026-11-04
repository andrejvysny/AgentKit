import { expect, test } from "bun:test";
import { cpSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NATIVE_PLATFORM_TARGETS } from "../../../rust/scripts/native-platform-targets.mjs";
import { NATIVE_ADDON_VERSION } from "../src/addon-version";

const packages = join(import.meta.dir, "../..");
function manifest(name: string): Record<string, unknown> {
  return JSON.parse(
    readFileSync(join(packages, name, "package.json"), "utf8"),
  ) as Record<string, unknown>;
}

const native = manifest("native");
const nativeVersion = String(native.version);

test("native stack is publishable, lockstep-versioned, and complete", () => {
  // The wrapper itself: no `private`, public access for the scoped name,
  // and a dist-pointing publishConfig (prepack promotes it verbatim).
  expect(native.private).toBeUndefined();
  expect(native.license).toBe("MIT");
  const publishConfig = native.publishConfig as Record<string, unknown>;
  expect(publishConfig.access).toBe("public");
  expect(publishConfig.exports).toBeDefined();
  expect(publishConfig.main).toBe("./dist/index.js");
  expect(publishConfig.types).toBe("./dist/index.d.ts");

  // The declared napi targets and the platform-package table agree exactly.
  const napiTargets = (native.napi as Record<string, unknown>)
    .targets as string[];
  expect([...napiTargets].sort()).toEqual(
    NATIVE_PLATFORM_TARGETS.map((target) => target.triple).sort(),
  );

  const optional = native.optionalDependencies as Record<string, string>;
  expect(Object.keys(optional).sort()).toEqual(
    NATIVE_PLATFORM_TARGETS.map((t) => `@agentkit/native-${t.key}`).sort(),
  );

  for (const target of NATIVE_PLATFORM_TARGETS) {
    const entry = manifest(target.package);
    expect(entry.name).toBe(`@agentkit/native-${target.key}`);
    // Lockstep: every platform package ships the wrapper's version; the
    // loader's version check depends on it.
    expect(entry.version).toBe(nativeVersion);
    expect(entry.private).toBeUndefined();
    expect((entry.publishConfig as Record<string, unknown>).access).toBe(
      "public",
    );
    expect(entry.os).toEqual([target.os]);
    expect(entry.cpu).toEqual([target.cpu]);
    expect(entry.libc).toEqual(target.libc ? [target.libc] : undefined);
    expect(entry.main).toBe(`agentkit-native.${target.key}.node`);
    expect(entry.files).toEqual([
      `agentkit-native.${target.key}.node`,
      "README.md",
    ]);
    expect(Object.keys(entry.scripts as object)).toEqual(["prepack"]);
    expect(entry.engines).toEqual(native.engines);
    expect((entry.repository as Record<string, unknown>).directory).toBe(
      `packages/${target.package}`,
    );
    // The wrapper pins each platform package at its exact version.
    expect(optional[`@agentkit/native-${target.key}`]).toBe(nativeVersion);
  }

  // The umbrella and `@agentkit/core` carry the same exact pins: an
  // installed `agentkit` must pull the addon whose version the loader
  // enforces, not an older one off the registry.
  const umbrella = manifest("agentkit");
  expect(umbrella.private).toBeUndefined();
  const umbrellaOptional = umbrella.optionalDependencies as Record<
    string,
    string
  >;
  for (const target of NATIVE_PLATFORM_TARGETS)
    expect(umbrellaOptional[`@agentkit/native-${target.key}`]).toBe(
      nativeVersion,
    );
  const core = manifest("core");
  expect(
    (core.optionalDependencies as Record<string, string>)["@agentkit/native"],
  ).toBe(nativeVersion);
});

test("committed loader enforces the wrapper's version on every platform", () => {
  const loader = readFileSync(
    join(packages, "native", "generated", "index.js"),
    "utf8",
  );
  const checks = [...loader.matchAll(/bindingPackageVersion !== '([^']+)'/g)];
  expect(checks.length).toBeGreaterThan(0);
  for (const match of checks) expect(match[1]).toBe(nativeVersion);
  // Each shipped platform package is actually reachable by the loader.
  for (const target of NATIVE_PLATFORM_TARGETS)
    expect(loader).toContain(`@agentkit/native-${target.key}`);
  // The generated loader's own checks are napi-rs opt-in (upstream shape,
  // left byte-identical so `build:native` regenerates nothing); real
  // enforcement lives in the WRAPPER loader below — pinned there, not here.
});

test("the wrapper's own version guard covers every triple in the table", () => {
  // src/loader.ts is the enforcement layer: it verifies every installed
  // platform package against the wrapper's version and THROWS. A sixth
  // triple must extend the mirror list or the guard silently misses it.
  const wrapperLoader = readFileSync(
    join(packages, "native", "src", "loader.ts"),
    "utf8",
  );
  for (const target of NATIVE_PLATFORM_TARGETS)
    expect(wrapperLoader).toContain(`"${target.key}"`);
  // A mismatch must be a THROWN load failure, never an ignored warning.
  expect(wrapperLoader).toContain("native binding package version mismatch");
  expect(wrapperLoader).toContain("throw new Error");
  expect(wrapperLoader).not.toContain("console.warn");
  // Exactly one documented escape hatch, matching the generated layer.
  expect(
    wrapperLoader.match(/NAPI_RS_ENFORCE_VERSION_CHECK/g)?.length,
  ).toBeGreaterThanOrEqual(1);
  expect(wrapperLoader).toContain('NAPI_RS_ENFORCE_VERSION_CHECK === "0"');
  // The guard compares against an embedded literal, never a runtime
  // `../package.json` read (the umbrella's dist/native has none).
  expect(wrapperLoader).not.toContain('"../package.json"');
  expect(NATIVE_ADDON_VERSION).toBe(nativeVersion);
});

test("platform pack refuses a missing addon", () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-platform-pack-"));
  try {
    cpSync(join(packages, "native-darwin-arm64"), directory, {
      recursive: true,
    });
    const result = Bun.spawnSync(["npm", "pack", "--dry-run"], {
      cwd: directory,
      env: { ...process.env, npm_config_cache: join(directory, "cache") },
      stderr: "pipe",
      stdout: "pipe",
    });
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain(
      "agentkit-native.darwin-arm64.node",
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
