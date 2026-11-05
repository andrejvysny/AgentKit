import { afterAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Repo-local guard for the packed-declaration regression class: the
 * published `@agentkit/native` declarations must resolve for a CLEAN
 * consumer — outside the workspace, `tsc --strict`, NodeNext resolution,
 * `skipLibCheck` OFF — not just for the monorepo, which always finds the
 * sibling `@agentkit/host` on disk. External review reproduced
 * `dist/assistant-store.d.ts(21): TS2307: Cannot find module
 * '@agentkit/host'` against the packed tarball because the shipped
 * declarations imported workspace-internal types the manifest only lists as
 * devDependencies; `scripts/prepack-bundle-types.mjs` rolls the entry point
 * up (api-extractor `bundledPackages`) so nothing workspace-internal can
 * leak. This test drives the REAL artifacts — the `build` pipeline output
 * and the REAL typed consumer — from a directory outside the repository.
 */

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const nativePackage = join(repoRoot, "packages", "native");
const temporary: string[] = [];

function stagingDirectory(): string {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-native-types-"));
  temporary.push(directory);
  return directory;
}

afterAll(() => {
  for (const directory of temporary.splice(0))
    rmSync(directory, { recursive: true, force: true });
});

function buildNative(entry: "package" | "root" = "package"): void {
  const build = spawnSync(
    process.execPath,
    entry === "package"
      ? ["run", "--cwd", nativePackage, "build"]
      : ["run", "build:native"],
    { cwd: repoRoot, encoding: "utf8" },
  );
  if (build.status !== 0)
    throw new Error(
      `native build failed (${build.status}):\n${build.stdout}\n${build.stderr}`,
    );
}

function declarationFiles(root: string): string[] {
  const found: string[] = [];
  const walk = (directory: string): void => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const absolute = join(directory, entry.name);
      if (entry.isDirectory()) walk(absolute);
      else if (entry.isFile() && entry.name.endsWith(".d.ts"))
        found.push(absolute);
    }
  };
  if (existsSync(root)) walk(root);
  return found;
}

const BARE_IMPORT = /(?:\bfrom|\bimport\s*\()\s*["']([^"'\s]+)["']/g;

function bareImportsIn(file: string): string[] {
  const text = readFileSync(file, "utf8");
  const found: string[] = [];
  BARE_IMPORT.lastIndex = 0;
  for (;;) {
    const match = BARE_IMPORT.exec(text);
    if (!match) break;
    const specifier = match[1];
    if (
      !specifier.startsWith(".") &&
      !specifier.startsWith("/") &&
      !specifier.startsWith("node:")
    )
      found.push(specifier);
  }
  return found;
}

describe("packed @agentkit/native declarations", () => {
  it(
    "root and package builds emit identical declarations and frozen API reports",
    () => {
      const report = join(
        repoRoot,
        "node_modules/.cache/agentkit-api/native.api.md",
      );
      const checkApi = (): string => {
        const result = spawnSync(
          "node",
          ["scripts/check-public-api.mjs", "--entry=native"],
          { cwd: repoRoot, encoding: "utf8" },
        );
        if (result.error || result.status !== 0)
          throw new Error(
            `native API check failed:\n${result.stdout}\n${result.stderr}`,
          );
        return readFileSync(report, "utf8");
      };
      buildNative("package");
      const declarations = readFileSync(
        join(nativePackage, "dist/index.d.ts"),
        "utf8",
      );
      const api = checkApi();
      buildNative("root");
      expect(readFileSync(join(nativePackage, "dist/index.d.ts"), "utf8")).toBe(
        declarations,
      );
      expect(checkApi()).toBe(api);
    },
    { timeout: 180_000 },
  );

  it(
    "build to a single self-contained dist entry point",
    () => {
      buildNative();
      const declarations = declarationFiles(join(nativePackage, "dist"));
      expect(declarations).toEqual([join(nativePackage, "dist", "index.d.ts")]);
      expect(bareImportsIn(declarations[0])).toEqual([]);
    },
    { timeout: 180_000 },
  );

  it(
    "type-check for a strict NodeNext consumer outside the workspace",
    () => {
      buildNative();
      const staging = stagingDirectory();
      // The install the consumer sees: the PACKED manifest shape
      // (publishConfig applied, exactly what `npm pack` tars) under
      // node_modules of a directory outside the repository.
      const stagedPackage = join(staging, "pkg");
      cpSync(nativePackage, stagedPackage, {
        recursive: true,
        filter: (source) =>
          !/[/\\](node_modules|tests)$/.test(source) &&
          !source.endsWith(".tgz"),
      });
      const apply = spawnSync(
        process.execPath,
        [join(repoRoot, "scripts", "prepack-publish-config.mjs"), "apply"],
        { cwd: stagedPackage, encoding: "utf8" },
      );
      if (apply.status !== 0)
        throw new Error(
          `prepack publishConfig swap failed:\n${apply.stdout}\n${apply.stderr}`,
        );
      const installed = join(
        staging,
        "consumer",
        "node_modules",
        "@agentkit",
        "native",
      );
      cpSync(stagedPackage, installed, { recursive: true });
      writeFileSync(
        join(staging, "consumer", "package.json"),
        `${JSON.stringify(
          {
            name: "packed-native-types-consumer",
            private: true,
            type: "module",
          },
          null,
          2,
        )}\n`,
      );
      cpSync(
        join(repoRoot, "rust", "scripts", "native-package-consumer.mjs"),
        join(staging, "consumer", "consumer.mjs"),
      );
      cpSync(
        join(repoRoot, "rust", "scripts", "native-package-typed-consumer.ts"),
        join(staging, "consumer", "native-package-typed-consumer.ts"),
      );
      const typed = spawnSync(
        "node",
        [
          "consumer.mjs",
          "types",
          join(repoRoot, "node_modules", "typescript", "bin", "tsc"),
        ],
        { cwd: join(staging, "consumer"), encoding: "utf8" },
      );
      expect(typed.stdout + typed.stderr).not.toContain("TS2307");
      if (typed.status !== 0)
        throw new Error(
          `strict typed consumer failed (${typed.status}):\n${typed.stdout}\n${typed.stderr}`,
        );
      expect(typed.stdout).toContain("mode=types");
    },
    { timeout: 180_000 },
  );
});
