import { afterAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  cpSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Guards for `scripts/prepack-bundle-types.mjs`, driving the REAL script
 * over synthetic workspace fixtures outside the repo. External review
 * (2026-11-02) reported four defects, each pinned by one case:
 *
 *  a) the workspace dependency graph must be BUILT before the declaration
 *     discovery/alias resolution runs — discovering first froze the alias
 *     map against a half-built workspace;
 *  b) a workspace package whose emitted declarations are missing must be
 *     REJECTED, not silently resolved through its package manifest into
 *     `./src/index.ts` (TypeScript source entering the extractor input
 *     set is what produced the undeclared `Type`/`AiContentPartSchema`
 *     references in the packed `dist/native/index.d.ts`);
 *  c) `Extractor.invoke` returns a result OBJECT (always truthy) — only
 *     `.succeeded` reports failure, so a failed analysis must exit 1
 *     instead of shipping the broken rollup;
 *  d) the finished rollup must typecheck STANDALONE (strict tsc,
 *     `skipLibCheck` off, `types: []`), the way a clean consumer reads it.
 */

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const script = join(repoRoot, "scripts", "prepack-bundle-types.mjs");
const temporary: string[] = [];

afterAll(() => {
  for (const directory of temporary.splice(0))
    rmSync(directory, { recursive: true, force: true });
});

interface Fixture {
  readonly root: string;
  readonly alpha: string;
  readonly beta: string;
}

/** A miniature workspace: `@agentkit/alpha` (packed here) imports
 * `@agentkit/beta` (dev-only, like native→host), linked through
 * `node_modules/@agentkit/beta` exactly like the real bun workspace. The
 * fixture's `node_modules` also symlinks the repo's `typescript` so
 * `ensureBuilt` finds the compiler. */
function fixture(options?: { betaTsconfig?: unknown }): Fixture {
  // realpath: macOS tmpdir() is a /var symlink to /private/var, and the
  // script reports resolved paths.
  const root = realpathSync(mkdtempSync(join(tmpdir(), "agentkit-prepack-")));
  temporary.push(root);
  const packages = join(root, "packages");
  const alpha = join(packages, "alpha");
  const beta = join(packages, "beta");
  mkdirSync(join(alpha, "dist"), { recursive: true });
  mkdirSync(join(beta, "src"), { recursive: true });
  const nodeModules = join(root, "node_modules");
  mkdirSync(join(nodeModules, "@agentkit"), { recursive: true });
  cpSync(
    join(repoRoot, "node_modules", "typescript"),
    join(nodeModules, "typescript"),
    { recursive: true },
  );
  writeFileSync(join(nodeModules, ".gitkeep"), "");
  // bun workspaces link siblings; emulate the symlink, not a copy, so
  // module resolution follows the SAME `exports` conditions a consumer of
  // the checkout sees (types → ./src/index.ts).
  writeFileSync(
    join(beta, "package.json"),
    `${JSON.stringify(
      {
        name: "@agentkit/beta",
        version: "0.0.0",
        type: "module",
        exports: {
          ".": { types: "./src/index.ts", import: "./src/index.ts" },
        },
      },
      null,
      2,
    )}\n`,
  );
  writeFileSync(
    join(beta, "src", "index.ts"),
    "export interface BetaThing {\n  beta: string;\n}\n",
  );
  writeFileSync(
    join(beta, "tsconfig.build.json"),
    `${JSON.stringify(
      options?.betaTsconfig ?? {
        compilerOptions: {
          target: "ES2022",
          module: "ESNext",
          moduleResolution: "Bundler",
          strict: true,
          declaration: true,
          emitDeclarationOnly: true,
          outDir: "dist",
          rootDir: "src",
        },
        include: ["src"],
      },
      null,
      2,
    )}\n`,
  );
  writeFileSync(
    join(alpha, "package.json"),
    `${JSON.stringify(
      {
        name: "@agentkit/alpha",
        version: "0.0.0",
        type: "module",
        devDependencies: { "@agentkit/beta": "workspace:*" },
      },
      null,
      2,
    )}\n`,
  );
  writeFileSync(join(alpha, "dist", "index.js"), "export const alpha = 1;\n");
  return { root, alpha, beta };
}

function linkBeta(root: string): void {
  // Symlink AFTER fixture() so the shared repo node_modules stays untouched.
  rmSync(join(root, "node_modules", "@agentkit"), {
    recursive: true,
    force: true,
  });
  mkdirSync(join(root, "node_modules", "@agentkit"), { recursive: true });
  symlinkSync(
    join(root, "packages", "beta"),
    join(root, "node_modules", "@agentkit", "beta"),
    "dir",
  );
}

function writeAlphaDeclarations(alpha: string, text: string): void {
  writeFileSync(join(alpha, "dist", "index.d.ts"), text);
}

function runPrepack(alpha: string) {
  return spawnSync(process.execPath, [script], {
    cwd: alpha,
    encoding: "utf8",
  });
}

const BARE_IMPORT = /(?:\bfrom|\bimport\s*\()\s*["']([^"'\s]+)["']/g;

function bareSpecifiers(file: string): string[] {
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

describe("prepack-bundle-types", () => {
  it(
    "builds unbuilt workspace dependencies BEFORE discovering declarations (a)",
    () => {
      const { root, alpha } = fixture();
      linkBeta(root);
      writeAlphaDeclarations(
        alpha,
        'export type { BetaThing } from "@agentkit/beta";\n' +
          "export declare const alpha: number;\n",
      );
      const result = runPrepack(alpha);
      expect(result.stdout + result.stderr).toContain(
        `building ${join(root, "packages", "beta")}`,
      );
      expect(result.status).toBe(0);
      const rolled = readFileSync(join(alpha, "dist", "index.d.ts"), "utf8");
      expect(rolled).toContain("interface BetaThing");
      // Self-contained: nothing bare, no per-module declarations remain.
      expect(bareSpecifiers(join(alpha, "dist", "index.d.ts"))).toEqual([]);
    },
    { timeout: 180_000 },
  );

  it(
    "rejects a workspace dependency whose emitted declarations are missing " +
      "instead of reading its TypeScript source (b)",
    () => {
      const { root, alpha } = fixture({
        betaTsconfig: {
          compilerOptions: {
            target: "ES2022",
            module: "ESNext",
            moduleResolution: "Bundler",
            strict: true,
            declaration: true,
            emitDeclarationOnly: true,
            // The regression shape: the build SUCCEEDS but never emits
            // dist/index.d.ts, so a stale/naive alias map would let module
            // resolution fall through to beta's exports `types` source.
            outDir: "typings",
            rootDir: "src",
          },
          include: ["src"],
        },
      });
      linkBeta(root);
      const before =
        'export type { BetaThing } from "@agentkit/beta";\n' +
        "export declare const alpha: number;\n";
      writeAlphaDeclarations(alpha, before);
      const result = runPrepack(alpha);
      expect(result.status).toBe(1);
      expect(`${result.stdout}${result.stderr}`).toContain("TypeScript source");
      // Nothing half-rolled was shipped in its place.
      expect(readFileSync(join(alpha, "dist", "index.d.ts"), "utf8")).toBe(
        before,
      );
    },
    { timeout: 180_000 },
  );

  it(
    "fails when api-extractor reports an unsuccessful run (.succeeded) (c)",
    () => {
      const { root, alpha } = fixture();
      linkBeta(root);
      // A syntax error tsc reports even with skipLibCheck on. The
      // `@agentkit/beta` import keeps the run on the ROLLUP branch (with
      // no bare specifier the script correctly exits as "already
      // self-contained" without invoking the extractor at all).
      const before =
        'export type { BetaThing } from "@agentkit/beta";\n' +
        "export const broken: = 5;\n";
      writeAlphaDeclarations(alpha, before);
      const result = runPrepack(alpha);
      expect(result.status).toBe(1);
      expect(`${result.stdout}${result.stderr}`).toContain(
        "api-extractor failed to roll up",
      );
      expect(readFileSync(join(alpha, "dist", "index.d.ts"), "utf8")).toBe(
        before,
      );
    },
    { timeout: 180_000 },
  );

  it(
    "rejects a rollup that does not typecheck standalone (d)",
    () => {
      const { root, alpha } = fixture();
      linkBeta(root);
      // The production shape: declarations that only pass because nobody
      // typechecks them. `skipLibCheck: true` masks semantic errors inside
      // .d.ts inputs during extraction, and api-extractor emits the AST
      // verbatim; a strict consumer (and the standalone gate) must not be
      // fooled. (An undeclared reference instead makes api-extractor throw;
      // both other failure modes are pinned above.)
      const before =
        'export type { BetaThing } from "@agentkit/beta";\n' +
        "export declare const alpha: number;\n" +
        "export type BrokenArity = Array<string, string>;\n";
      writeAlphaDeclarations(alpha, before);
      const result = runPrepack(alpha);
      expect(result.status).toBe(1);
      expect(`${result.stdout}${result.stderr}`).toContain(
        "do not typecheck standalone",
      );
      expect(readFileSync(join(alpha, "dist", "index.d.ts"), "utf8")).toBe(
        before,
      );
    },
    { timeout: 180_000 },
  );
});
