/**
 * Behavioural tests for the wrapper's ABI-skew guard
 * (`assertAddonVersionsMatchWrapper` in src/loader.ts): a platform package
 * whose version drifted from the wrapper must turn `loadBinding()` into a
 * rejected load with a precise message, and the one documented escape
 * hatch (`NAPI_RS_ENFORCE_VERSION_CHECK=0`) must be the only way past it.
 *
 * Each case builds a COMPLETELY isolated tmp project
 * (`tmp/node_modules/@agentkit/...` + a copy of the wrapper source tree),
 * so module resolution can never fall back into the repo's workspace
 * links and no fixture can shadow the repo's own node_modules for other
 * test files. The addon binary is the real committed linux-x64-gnu build;
 * the guard fires on the package.json version before any of that matters.
 */
import {
  chmodSync,
  copyFileSync,
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
import { afterAll, describe, expect, test } from "bun:test";

const repoRoot = join(import.meta.dir, "..", "..", "..");
const nativeVersion = JSON.parse(
  readFileSync(join(repoRoot, "packages/native/package.json"), "utf8"),
).version as string;

// The guard can only fire on the platform whose addon this host can
// actually load; the generated loader hard-requires the host triple's
// package. Other cells are structural-checked by distribution.test.ts.
const HOST_KEY =
  process.platform === "linux" && process.arch === "x64"
    ? "linux-x64-gnu"
    : process.platform === "linux" && process.arch === "arm64"
      ? "linux-arm64-gnu"
      : process.platform === "darwin" && process.arch === "arm64"
        ? "darwin-arm64"
        : process.platform === "darwin" && process.arch === "x64"
          ? "darwin-x64"
          : process.platform === "win32" && process.arch === "x64"
            ? "win32-x64-msvc"
            : null;

const cleanups: string[] = [];
afterAll(() => {
  for (const dir of cleanups) rmSync(dir, { recursive: true, force: true });
});

/** Build tmp/node_modules/@agentkit/{native,native-<key>} with the
 * platform package at `platformVersion`, and a consumer entry importing
 * the wrapper by package specifier. Returns the consumer file path. */
function isolatedProject(platformVersion: string | null): string {
  const root = mkdtempSync(join(tmpdir(), "agentkit-version-guard-"));
  cleanups.push(root);
  const scope = join(root, "node_modules", "@agentkit");
  mkdirSync(scope, { recursive: true });
  // The wrapper: a copy of the real source tree (src + generated +
  // package.json), so the guard's relative requires match production.
  const wrapper = join(scope, "native");
  cpSync(join(repoRoot, "packages/native"), wrapper, {
    recursive: true,
    // Drop node_modules AND the committed local addon fallback
    // (`generated/*.node`): the ONLY addon path in the isolated project
    // must be the platform package fixture under test.
    filter: (src) => !src.includes("node_modules") && !src.endsWith(".node"),
  });
  rmSync(join(wrapper, "node_modules"), { recursive: true, force: true });
  // Bun honours `links` and workspace-protocol deps by walking back to the
  // repo — the tmp copy must resolve ONLY inside tmp.
  const wrapperManifestPath = join(wrapper, "package.json");
  const wrapperManifest = JSON.parse(readFileSync(wrapperManifestPath, "utf8"));
  delete wrapperManifest.links;
  delete wrapperManifest.workspaces;
  writeFileSync(
    wrapperManifestPath,
    `${JSON.stringify(wrapperManifest, null, 2)}\n`,
  );
  // The platform package at the requested version, with a REAL addon at
  // its package root (that is what `main` names; in the dev checkout the
  // committed `generated/*.node` fallback serves that role, and the
  // staging script copies the cargo build there for release).
  if (platformVersion !== null) {
    const platform = join(scope, `native-${HOST_KEY}`);
    cpSync(join(repoRoot, `packages/native-${HOST_KEY}`), platform, {
      recursive: true,
    });
    const addon = join(
      repoRoot,
      "packages/native/generated",
      `agentkit-native.${HOST_KEY}.node`,
    );
    if (!existsSync(addon))
      throw new Error(`no committed addon to fixture with: ${addon}`);
    copyFileSync(addon, join(platform, `agentkit-native.${HOST_KEY}.node`));
    const manifestPath = join(platform, "package.json");
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    manifest.version = platformVersion;
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  }
  const consumer = join(root, "consumer.ts");
  writeFileSync(
    consumer,
    `const { NativeStore } = await import("@agentkit/native");
try {
  const store = await NativeStore.inMemory();
  await store.close?.();
  console.log("LOADED");
} catch (error) {
  console.log("REJECT:" + String(error instanceof Error ? error.message : error));
}\n`,
  );
  chmodSync(consumer, 0o644);
  return consumer;
}

function runConsumer(consumer: string, env: Record<string, string> = {}) {
  const result = Bun.spawnSync(["bun", "run", consumer], {
    cwd: join(consumer, ".."),
    env: { ...process.env, ...env },
    stderr: "pipe",
    stdout: "pipe",
  });
  return result.stdout.toString();
}

describe.skipIf(HOST_KEY === null)("wrapper version guard", () => {
  test("matching versions load normally", () => {
    const out = runConsumer(isolatedProject(nativeVersion));
    expect(out).toContain("LOADED");
  });

  test("a drifted platform version rejects the load, loudly", () => {
    const out = runConsumer(isolatedProject("9.9.9"));
    expect(out).toContain("REJECT:");
    expect(out).toContain("native binding package version mismatch");
    expect(out).toContain(`@agentkit/native-${HOST_KEY}`);
    expect(out).toContain("9.9.9");
    expect(out).toContain(nativeVersion);
    expect(out).not.toContain("LOADED");
  });

  test("the documented escape hatch is the only way past the guard", () => {
    // Garbage values do NOT disable it (napi-rs convention: only "0").
    expect(
      runConsumer(isolatedProject("9.9.9"), {
        NAPI_RS_ENFORCE_VERSION_CHECK: "1",
      }),
    ).toContain("REJECT:");
    expect(
      runConsumer(isolatedProject("9.9.9"), {
        NAPI_RS_ENFORCE_VERSION_CHECK: "",
      }),
    ).toContain("REJECT:");
    expect(
      runConsumer(isolatedProject("9.9.9"), {
        NAPI_RS_ENFORCE_VERSION_CHECK: "0",
      }),
    ).toContain("LOADED");
  });

  test("a missing platform package is not a version failure (foreign cell)", () => {
    // The guard skips what cannot resolve; the generated loader then
    // produces the ordinary "cannot find native binding" failure for the
    // HOST triple — a different, load-shaped error, never the mismatch.
    const out = runConsumer(isolatedProject(null));
    expect(out).toContain("REJECT:");
    expect(out).not.toContain("package version mismatch");
  });
});
