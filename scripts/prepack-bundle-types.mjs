#!/usr/bin/env node
/**
 * Ship self-contained declarations: roll the package's `dist/index.d.ts` up
 * with `@microsoft/api-extractor` (the tool `scripts/check-public-api.mjs`
 * already uses for the frozen public API reports) so the packed tarball
 * resolves for a clean consumer install WITHOUT `skipLibCheck`.
 *
 * The monorepo masks a classic packaging bug: `@agentkit/native`'s emitted
 * declarations used `import type { … } from "@agentkit/host"` while listing
 * host only as a devDependency. Inside the workspace the sibling resolves
 * from disk; for a consumer of the packed tarball, devDependencies are never
 * installed and strict `tsc` fails with
 * `TS2307: Cannot find module '@agentkit/host' or its corresponding type
 * declarations` (reproduced by external review with a packed + separately
 * installed consumer under `--moduleResolution nodenext`). Descending past
 * host drags in `@agentkit/core`/`@agentkit/contracts`, whose reachable
 * types reference `@sinclair/typebox` — a third-party type leak the plain
 * copy-tree approach cannot hide either.
 *
 * This script runs after `tsc` (package `build`) and again as the first
 * half of `prepack` (so `npm pack`/`npm publish` can never ship an
 * unrolled entry point, even from a stale build):
 *
 *   1. Every `@agentkit/*` package the emitted declarations import that is
 *      not a runtime-provided dependency (`dependencies`,
 *      `peerDependencies`, `optionalDependencies` — what npm actually
 *      installs) becomes an api-extractor `bundledPackages` entry, so its
 *      reachable declarations are INLINED into `dist/index.d.ts`. Workspace
 *      type packages must never become runtime dependencies just to satisfy
 *      types; the published dependency graph stays exactly as declared.
 *   2. A third-party type that still leaks through the rollup (typebox's
 *      `TObject<…>` schema types are the case here) is added to
 *      `bundledPackages` too and re-rolled, until the entry point imports
 *      NOTHING bare. Any specifier that cannot be bundled fails loudly —
 *      a consumer would hit TS2307 on it.
 *   3. Emitted JS that imports a workspace package npm will not install is
 *      a runtime bug (not just a types bug), so it fails here too.
 *   4. After the rollup, the per-module `dist/` `.d.ts` files — the dead
 *      multi-file declarations the rolled-up entry replaces — are removed
 *      along with their declaration maps. The `.js` outputs are untouched;
 *      the package's `exports` only ever advertise `dist/index.d.ts`.
 *
 * Workspace packages whose `dist` is missing or older than their `src` are
 * rebuilt first (with the workspace `typescript`) — inlining a stale
 * `dist/index.d.ts` would be worse than not building.
 *
 * In a staged single-package copy without sibling packages
 * (`rust/scripts/check-native-package.mjs` packs from staging) the entry is
 * already rolled up by the preceding build; the script detects the
 * self-contained state, verifies the entry imports nothing bare, and exits
 * without touching anything.
 */
// The rollup branch resolves api-extractor from the repository that owns
// the checkout — the staged single-package copy outside the repo never
// reaches it (its dist arrived pre-rolled), so the import must stay lazy.
async function loadExtractor() {
  try {
    return await import("@microsoft/api-extractor");
  } catch (error) {
    fail(
      `cannot load @microsoft/api-extractor to roll up the declarations ` +
        `(${error instanceof Error ? error.message : String(error)}); pack ` +
        "must run from the repository after `bun install`",
    );
  }
}
import { spawnSync } from "node:child_process";
import {
  existsSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, relative, resolve, sep } from "node:path";

const WORKSPACE_SCOPE = "@agentkit/";

function fail(message) {
  process.stderr.write(
    `prepack-bundle-types: ${message}\n` +
      "the packed declarations would not resolve for a clean consumer install " +
      "(TS2307 without skipLibCheck).\n",
  );
  process.exit(1);
}

const packageDirectory = process.cwd();
const manifest = JSON.parse(
  readFileSync(join(packageDirectory, "package.json"), "utf8"),
);
const ownName = manifest.name;
const distDirectory = join(packageDirectory, "dist");
const entryDeclarations = join(distDirectory, "index.d.ts");

if (!existsSync(distDirectory))
  fail(
    `no dist/ directory in ${packageDirectory}; run the package build first`,
  );
if (!existsSync(entryDeclarations))
  fail(`dist/index.d.ts is missing; run the package build first`);

// Dependency fields npm actually installs for a consumer of this package.
// Their types may keep importing these by name; anything else (dev,
// test-only workspace peers, unlisted siblings) must be inlined.
const runtimeProvided = new Set([
  ...Object.keys(manifest.dependencies ?? {}),
  ...Object.keys(manifest.peerDependencies ?? {}).filter(
    (name) => manifest.peerDependenciesMeta?.[name]?.optional !== true,
  ),
  ...Object.keys(manifest.optionalDependencies ?? {}),
]);

function walkFiles(root, extensions) {
  const found = [];
  const walk = (directory) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const absolute = join(directory, entry.name);
      if (entry.isDirectory()) walk(absolute);
      else if (entry.isFile() && extensions.some((x) => entry.name.endsWith(x)))
        found.push(absolute);
    }
  };
  if (existsSync(root)) walk(root);
  return found;
}

const SPECIFIER_IN_CONTEXT = /(?:\bfrom|\bimport\s*\()\s*["']([^"'\s]+)["']/g;

/** Package name of a bare specifier, or undefined for relative/builtin. */
function barePackageName(specifier) {
  if (!specifier) return undefined;
  if (specifier.startsWith(".") || specifier.startsWith("/")) return undefined;
  if (specifier.startsWith("node:")) return undefined;
  const withoutQuery = specifier.split("?")[0];
  const segments = withoutQuery.split("/");
  return withoutQuery.startsWith("@")
    ? segments.slice(0, 2).join("/")
    : segments[0];
}

function bareSpecifiersIn(text) {
  const found = new Set();
  SPECIFIER_IN_CONTEXT.lastIndex = 0;
  for (;;) {
    const match = SPECIFIER_IN_CONTEXT.exec(text);
    if (!match) break;
    const name = barePackageName(match[1]);
    if (name) found.add(name);
  }
  return found;
}

// ---------------------------------------------------------------------------
// 1. Emitted JS must never import a workspace package npm will not install.
// ---------------------------------------------------------------------------
for (const file of walkFiles(distDirectory, [".js"])) {
  for (const specifier of [
    ...readFileSync(file, "utf8").matchAll(
      /(?:\bfrom|\bimport\s*\()\s*["']((?:@agentkit\/)[^"'\s]+)["']/g,
    ),
  ]) {
    const name = barePackageName(specifier[1]);
    if (name && name !== ownName && !runtimeProvided.has(name))
      fail(
        `dist/${relative(distDirectory, file).split(sep).join("/")} imports ` +
          `${name} at runtime but it is not a (peer/optional) dependency of ` +
          `${ownName} — an installed consumer would crash, not just fail tsc`,
      );
  }
}

/** Resolve `@agentkit/<name>` to an on-disk workspace package directory,
 * walking ancestors of the package being packed (same lookup as
 * prepack-publish-config.mjs, so staging trees behave identically). */
function findWorkspacePackage(name) {
  const shortName = name.slice(WORKSPACE_SCOPE.length);
  let current = resolve(packageDirectory);
  for (;;) {
    for (const candidateDir of [
      join(current, "packages", shortName),
      join(current, "packages", name),
    ]) {
      const manifestPath = join(candidateDir, "package.json");
      if (existsSync(manifestPath)) {
        try {
          if (JSON.parse(readFileSync(manifestPath, "utf8")).name === name)
            return candidateDir;
        } catch {
          /* keep walking */
        }
      }
    }
    const parent = dirname(current);
    if (parent === current) return undefined;
    current = parent;
  }
}

function typescriptCompiler() {
  const require = createRequire(join(packageDirectory, "noop.js"));
  try {
    const main = require.resolve("typescript");
    const bin = join(dirname(dirname(main)), "bin", "tsc");
    return existsSync(bin) ? bin : undefined;
  } catch {
    return undefined;
  }
}

/** Build a workspace package's dist with its own tsconfig.build.json when
 * missing or older than its sources. */
function ensureBuilt(packagePath) {
  const distIndex = join(packagePath, "dist", "index.d.ts");
  const sourceDirectory = join(packagePath, "src");
  const sources = existsSync(sourceDirectory)
    ? walkFiles(sourceDirectory, [".ts", ".tsx"])
    : [];
  let needsBuild = !existsSync(distIndex);
  if (!needsBuild && sources.length > 0) {
    const built = statSync(distIndex).mtimeMs;
    needsBuild = sources.some((file) => statSync(file).mtimeMs > built);
  }
  if (!needsBuild) return;
  const tsconfig = join(packagePath, "tsconfig.build.json");
  if (!existsSync(tsconfig))
    fail(
      `${packagePath} has no tsconfig.build.json to build the ` +
        "declarations being inlined; build its dist before packing this package",
    );
  const tsc = typescriptCompiler();
  if (!tsc)
    fail(
      "cannot resolve the workspace `typescript` compiler to refresh a " +
        "stale inlined package dist",
    );
  process.stdout.write(
    `prepack-bundle-types: building ${packagePath} (inlined declarations ` +
      "missing or older than its sources)…\n",
  );
  const result = spawnSync(process.execPath, [tsc, "-p", tsconfig], {
    cwd: packagePath,
    stdio: "inherit",
  });
  if (result.status !== 0)
    fail(`building ${packagePath} failed with status ${result.status}`);
}

function resolveFromPackage(name) {
  const require = createRequire(join(packageDirectory, "noop.js"));
  try {
    require.resolve(name);
    return true;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// 2. Roll the entry point up with api-extractor until nothing bare remains.
// ---------------------------------------------------------------------------

/** Enumerate workspace `@agentkit/*` packages whose declarations are built.
 * MUST be called AFTER `ensureBuilt` — discovering earlier freezes the alias
 * map against a half-built workspace, and any package missing from `paths`
 * then resolves through `node_modules` symlinks into the sibling's `exports`
 * `types` condition, which in this monorepo points AT THE SOURCE
 * (`./src/index.ts`). api-extractor would happily read TypeScript source
 * into the rollup input set; see the source-.ts rejection below. */
function discoverWorkspacePackages() {
  return readdirSync(join(dirname(packageDirectory)))
    .filter((name) =>
      existsSync(join(dirname(packageDirectory), name, "dist", "index.d.ts")),
    )
    .map((name) => `@agentkit/${name}`)
    .filter((name) => name !== ownName);
}

/** The `paths` alias map for one extractor pass: every built workspace
 * sibling pinned to its emitted `dist/index.d.ts`. Names whose manifest
 * says they are NOT `@agentkit/<name>` (the umbrella directory, say) get no
 * alias — they are not resolvable under that specifier anyway. */
function aliasMap() {
  const entries = [];
  for (const name of discoverWorkspacePackages()) {
    const packagePath = findWorkspacePackage(name);
    if (!packagePath) continue;
    entries.push([name, [join(packagePath, "dist", "index.d.ts")]]);
  }
  return Object.fromEntries(entries);
}

/** Every file api-extractor is about to read, explicitly: the entry point
 * plus every alias target. Anything that is not an existing `.d.ts` under
 * the owning package's `dist/` is a stale-discovery or misconfigured-build
 * bug whose fallback resolution is the sibling's TypeScript SOURCE — the
 * exact failure that once made the packed rollup contain undeclared
 * `Type`/`AiContentPartSchema` references (source-level names that tsc had
 * never emitted). Reject, never paper over. */
function assertDeclarationInputs(entryPath, paths) {
  for (const file of [entryPath, ...Object.values(paths).flat()]) {
    if (!existsSync(file))
      fail(
        `extractor input ${file} does not exist; a workspace dependency's ` +
          "build did not emit it — refusing to let module resolution fall " +
          "back to that package's TypeScript source",
      );
    if (file.endsWith(".d.ts")) continue;
    fail(
      `extractor input ${file} is not a declaration file; the rollup must ` +
        "never read TypeScript source (.ts) — rebuild the dependency's dist",
    );
  }
  // A workspace package imported by the inputs but ABSENT from the alias
  // map resolves through `node_modules` into the package manifest's `types`
  // condition — `./src/index.ts` in this monorepo. Source entering the
  // extractor input set is the same regression class, so it is refused too.
  for (const file of walkFiles(distDirectory, [".d.ts"])) {
    for (const name of bareSpecifiersIn(readFileSync(file, "utf8"))) {
      if (runtimeProvided.has(name) || !name.startsWith(WORKSPACE_SCOPE))
        continue;
      if (paths[name]) continue;
      const packagePath = findWorkspacePackage(name);
      if (packagePath)
        fail(
          `the extractor input set imports ${name} with no dist ` +
            `declaration alias; module resolution would read ` +
            `${join(packagePath, "src")} (TypeScript source) — build or ` +
            "fix its tsconfig.build.json so dist/index.d.ts exists",
        );
    }
  }
}

/** tsc over the finished rollup, standalone: strict, no `skipLibCheck`, no
 * lib/module resolution beyond what the rollup itself declares. The packed
 * consumer typechecks this exact text with `skipLibCheck` off
 * (`rust/scripts/check-native-package.mjs`, `scripts/tests/
 * packed-native-types.test.ts`); doing it here turns "the extractor emitted
 * something broken" into a pack-time failure instead of a consumer bug. */
function verifyRolledDeclarations(rollupPath) {
  const tsc = typescriptCompiler();
  if (!tsc)
    fail(
      "cannot resolve the workspace `typescript` compiler to verify the " +
        "rolled-up declarations",
    );
  // A dedicated tsconfig with `types: []`: the check must cover exactly
  // what the consumer reads — the rollup text — without ambient @types
  // from the monorepo flattering or sabotaging it.
  const tsconfigPath = join(distDirectory, ".index.rollup.tsconfig.json");
  writeFileSync(
    tsconfigPath,
    `${JSON.stringify(
      {
        compilerOptions: {
          target: "ES2022",
          module: "ESNext",
          moduleResolution: "Bundler",
          lib: ["ES2022", "DOM", "DOM.Iterable"],
          strict: true,
          skipLibCheck: false,
          types: [],
          noEmit: true,
        },
        files: [relative(distDirectory, rollupPath)],
      },
      null,
      2,
    )}\n`,
  );
  const check = spawnSync(process.execPath, [tsc, "-p", tsconfigPath], {
    cwd: packageDirectory,
    stdio: "inherit",
  });
  rmSync(tsconfigPath, { force: true });
  if (check.status !== 0)
    fail(
      `the rolled-up declarations do not typecheck standalone ` +
        `(tsc status ${check.status}) — undeclared references or illegal ` +
        "ambient initializers would break every strict consumer",
    );
}

const declarationTexts = walkFiles(distDirectory, [".d.ts"]);
const entryBare = new Set();
for (const file of declarationTexts) {
  for (const name of bareSpecifiersIn(readFileSync(file, "utf8"))) {
    if (!runtimeProvided.has(name)) entryBare.add(name);
  }
}

if (entryBare.size === 0) {
  // Already rolled up (a repeated prepack, or the staged single-package
  // copy where the preceding in-repo build did the work). Verify the
  // self-contained state holds, then stop.
  for (const file of declarationTexts) {
    const unresolved = [...bareSpecifiersIn(readFileSync(file, "utf8"))].filter(
      (name) => !runtimeProvided.has(name),
    );
    if (unresolved.length > 0)
      fail(
        `dist/${relative(distDirectory, file).split(sep).join("/")} still ` +
          `imports ${unresolved.join(", ")} — not a runtime dependency and ` +
          "not inlined; re-run the package build",
      );
  }
  process.stdout.write(
    "prepack-bundle-types: dist/index.d.ts is already rolled up and " +
      "self-contained; nothing to do\n",
  );
  process.exit(0);
}

const bundled = new Set();

/** Pull `name` into the bundle, building its workspace dist first when
 * needed. Runs BEFORE the alias map is computed (and is re-run for every
 * leak), so discovery never freezes against an unbuilt dependency. */
function requireBundled(name) {
  if (bundled.has(name)) return;
  if (!name.startsWith(WORKSPACE_SCOPE)) {
    if (!resolveFromPackage(name))
      fail(
        `entry imports ${name} but it is not installed and not a runtime ` +
          "dependency of the package",
      );
    bundled.add(name);
    return;
  }
  const packagePath = findWorkspacePackage(name);
  if (!packagePath)
    fail(
      `${name} is imported by the emitted declarations but is neither a ` +
        `runtime dependency of ${ownName} nor an on-disk workspace package; ` +
        "the staged pack must run after the in-repo build",
    );
  ensureBuilt(packagePath);
  bundled.add(name);
}

for (const name of entryBare) requireBundled(name);

const rollupTemp = join(distDirectory, ".index.rollup.d.ts");
const { Extractor, ExtractorConfig } = await loadExtractor();
let pass = 0;
for (;;) {
  pass += 1;
  // Recomputed every pass, AFTER `requireBundled` built whatever this pass
  // needs: the alias map must pin each workspace package to its emitted
  // `dist/index.d.ts`, never to a state where resolution could fall back
  // into TypeScript source.
  const paths = aliasMap();
  assertDeclarationInputs(entryDeclarations, paths);
  const config = ExtractorConfig.prepare({
    configObject: {
      projectFolder: packageDirectory,
      mainEntryPointFilePath: entryDeclarations,
      bundledPackages: [...bundled],
      compiler: {
        overrideTsconfig: {
          compilerOptions: {
            target: "ES2022",
            module: "ESNext",
            moduleResolution: "Bundler",
            lib: ["ES2022", "DOM", "DOM.Iterable"],
            strict: true,
            skipLibCheck: true,
            types: [],
            paths,
          },
          files: [entryDeclarations],
        },
      },
      apiReport: { enabled: false },
      docModel: { enabled: false },
      tsdocMetadata: { enabled: false },
      dtsRollup: { enabled: true, untrimmedFilePath: rollupTemp },
      newlineKind: "lf",
      messages: {
        extractorMessageReporting: {
          default: { logLevel: "warning" },
          "ae-missing-release-tag": { logLevel: "none" },
          "ae-forgotten-export": { logLevel: "none" },
          "ae-undocumented": { logLevel: "none" },
          "ae-unresolved-link": { logLevel: "none" },
        },
        tsdocMessageReporting: { default: { logLevel: "none" } },
      },
    },
    configObjectFullPath: join(packageDirectory, "api-extractor.json"),
    packageJsonFullPath: join(packageDirectory, "package.json"),
  });
  // `Extractor.invoke` returns an ExtractorResult OBJECT, which is always
  // truthy — the only failure signal is `.succeeded` — and it can also
  // THROW outright (`InternalError: Unable to follow symbol …`) on broken
  // inputs. Checking the object instead let a failed analysis ship a
  // rollup with undeclared references (a consumer's `TS2304 Cannot find
  // name` bug, silent at pack time). Both failure modes must abort pack.
  let result;
  let thrown;
  try {
    result = Extractor.invoke(config, {
      localBuild: true,
      showVerboseMessages: false,
    });
  } catch (error) {
    thrown = error instanceof Error ? error.message : String(error);
  }
  if (thrown !== undefined)
    fail(`api-extractor failed to roll up dist/index.d.ts (threw: ${thrown})`);
  if (!result?.succeeded)
    fail("api-extractor failed to roll up dist/index.d.ts");

  const rolled = readFileSync(rollupTemp, "utf8");
  const leaked = [...bareSpecifiersIn(rolled)].filter(
    (name) => !runtimeProvided.has(name),
  );
  if (leaked.length === 0) break;
  if (pass >= 5)
    fail(
      `the rolled-up entry still imports ${leaked.join(", ")} after ${pass} bundling passes`,
    );
  for (const name of leaked) {
    if (name.startsWith(WORKSPACE_SCOPE)) {
      const packagePath = findWorkspacePackage(name);
      if (!packagePath)
        fail(
          `${name} appeared in the rollup but is not an on-disk workspace package`,
        );
      ensureBuilt(packagePath);
    } else if (!resolveFromPackage(name)) {
      fail(
        `the rolled-up entry still imports ${name}, which is neither a ` +
          "runtime dependency of the package nor installed — a consumer " +
          "would hit TS2307 on it",
      );
    }
    bundled.add(name);
    process.stdout.write(
      `prepack-bundle-types: ${name} leaked into the rollup; bundling it\n`,
    );
  }
}

// The finished rollup must stand on its own: strict tsc with
// `skipLibCheck` off, exactly like the installed-consumer qualification
// (`rust/scripts/check-native-package.mjs`). A rollup that only "passes"
// because nobody read it is how undeclared references and illegal ambient
// initializers reached the packed tarball once already.
verifyRolledDeclarations(rollupTemp);

// ---------------------------------------------------------------------------
// 3. Swap in the rollup; drop the dead per-module declarations it replaces.
// ---------------------------------------------------------------------------
const rolledText = readFileSync(rollupTemp, "utf8");
rmSync(rollupTemp, { force: true });
for (const file of walkFiles(distDirectory, [".d.ts", ".d.ts.map"]))
  rmSync(file);
writeFileSync(entryDeclarations, rolledText);
process.stdout.write(
  `prepack-bundle-types: dist/index.d.ts is a self-contained rollup ` +
    `(bundled: ${[...bundled].join(", ") || "none"}); per-module ` +
    "declarations removed\n",
);
