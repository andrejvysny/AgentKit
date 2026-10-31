import { expect, test } from "bun:test";
import { cpSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const packages = join(import.meta.dir, "../..");
function manifest(name: string): Record<string, unknown> {
  return JSON.parse(
    readFileSync(join(packages, name, "package.json"), "utf8"),
  ) as Record<string, unknown>;
}

test("native optional packages use exact matching private platform metadata", () => {
  const native = manifest("native");
  const dependencies = native.optionalDependencies as Record<string, string>;
  const version = String(native.version);
  for (const [target, os, cpu, libc] of [
    ["darwin-arm64", "darwin", "arm64", undefined],
    ["linux-x64-gnu", "linux", "x64", "glibc"],
    ["win32-x64-msvc", "win32", "x64", undefined],
  ]) {
    const entry = manifest(`native-${target}`);
    expect(entry.name).toBe(`@agentkit/native-${target}`);
    expect(entry.version).toBe(version);
    expect(entry.private).toBe(true);
    expect(dependencies[String(entry.name)]).toBe(version);
    expect(entry.os).toEqual([os]);
    expect(entry.cpu).toEqual([cpu]);
    expect(entry.libc).toEqual(libc ? [libc] : undefined);
    expect(entry.main).toBe(`agentkit-native.${target}.node`);
    expect(Object.keys(entry.scripts as object)).toEqual(["prepack"]);
    expect(entry.engines).toEqual(native.engines);
  }
  expect(native.private).toBe(true);
  expect(native.files).not.toContain("generated/*.node");
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
