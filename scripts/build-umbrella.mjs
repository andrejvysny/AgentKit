#!/usr/bin/env node
/**
 * Assembles `packages/agentkit/dist` from the ten source packages' own
 * builds, producing the `agentkit` umbrella package — the single
 * installable artifact this repo ships (see `packages/agentkit/README.md`).
 *
 * This script only copies and rewrites already-built dists; it does not
 * compile anything itself.
 *
 *   node scripts/build-umbrella.mjs      # or: bun run build:umbrella
 *
 * Run `bun run build` first — it reads `packages/<p>/dist`, which is not
 * checked in.
 *
 * What it does, in order:
 *
 *   1. Asserts every source package's `dist/index.js` exists.
 *   2. Computes the union of the ten packages' runtime `dependencies`
 *      (excluding `@agentkit/*`) and cross-checks it against
 *      `packages/agentkit/package.json`'s own `dependencies` — a
 *      conflicting range between two source packages, or drift between the
 *      computed union and what the umbrella manifest declares, is a hard
 *      error. This is the guard against a package quietly gaining or
 *      changing a third-party dependency that the umbrella never picks up.
 *   3. Wipes and recreates `packages/agentkit/dist`.
 *   4. Copies each `packages/<p>/dist` -> `packages/agentkit/dist/<p>`,
 *      including non-JS runtime assets (e.g. testing's golden-trace
 *      JSONs), excluding `.map` files. Every copied `.js`/`.d.ts` file has:
 *        - its `//# sourceMappingURL=` line stripped (the `.map` it
 *          pointed at was just excluded, so the comment would dangle), and
 *        - every `@agentkit/<pkg>` module specifier (`from "..."`,
 *          `import("...")`, `export ... from "..."`) rewritten to the
 *          correct relative path into `dist/<pkg>/index.js`, computed from
 *          that file's own directory — then any leftover `@agentkit/` text
 *          (e.g. inside a doc comment) is collapsed to `agentkit/` so the
 *          old scope cannot survive anywhere in the shipped dist. A leftover
 *          that is still SPECIFIER-shaped (a quoted subpath import, a
 *          template literal) fails the build with the file and line instead
 *          of being collapsed into a broken bare import — see
 *          `./umbrella-specifiers.mjs`.
 *   5. Verifies no `@agentkit/` string remains anywhere in the umbrella
 *      dist, and that every `exports` entry in
 *      `packages/agentkit/package.json` resolves to a file that actually
 *      exists. Exits non-zero on any failure.
 */
import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  AGENTKIT_SPECIFIER,
  findResidualSpecifiers,
} from "./umbrella-specifiers.mjs";

const ROOT = fileURLToPath(new URL("../", import.meta.url));
const UMBRELLA_DIR = join(ROOT, "packages", "agentkit");
const UMBRELLA_DIST = join(UMBRELLA_DIR, "dist");

/**
 * Subpath name == source package directory name == the suffix of
 * "@agentkit/<name>" == the key in `exports` (minus the leading "./"), in
 * the exact order `packages/agentkit/package.json`'s `exports` lists them.
 */
const SUBPATHS = [
  "contracts",
  "core",
  "host",
  "testing",
  "mcp-client",
  "transport-http",
  "mcp-server",
  "adapters-memory",
  "adapters-sqlite",
  "runner-local",
];

function fail(message) {
  console.error(`build-umbrella: ${message}`);
  process.exit(1);
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

// ---------------------------------------------------------------------------
// 1. Source dists must exist
// ---------------------------------------------------------------------------

for (const name of SUBPATHS) {
  const entry = join(ROOT, "packages", name, "dist", "index.js");
  if (!existsSync(entry)) {
    fail(
      `packages/${name}/dist/index.js is missing — run "bun run build" first.`,
    );
  }
}

// ---------------------------------------------------------------------------
// 2. Dependency union + drift check
// ---------------------------------------------------------------------------

const union = new Map(); // depName -> { range, from: sourcePackageName }
for (const name of SUBPATHS) {
  const pkg = readJson(join(ROOT, "packages", name, "package.json"));
  for (const [depName, range] of Object.entries(pkg.dependencies ?? {})) {
    if (depName.startsWith("@agentkit/")) continue;
    const existing = union.get(depName);
    if (existing && existing.range !== range) {
      fail(
        `dependency "${depName}" is pinned to conflicting ranges: ` +
          `"${existing.range}" (from @agentkit/${existing.from}) vs ` +
          `"${range}" (from @agentkit/${name}). Align the ranges before ` +
          "building the umbrella.",
      );
    }
    if (!existing) union.set(depName, { range, from: name });
  }
}

const umbrellaPkgPath = join(UMBRELLA_DIR, "package.json");
const umbrellaPkg = readJson(umbrellaPkgPath);
const declared = umbrellaPkg.dependencies ?? {};

const driftMessages = [];
for (const [depName, { range }] of union) {
  if (declared[depName] !== range) {
    driftMessages.push(
      `  ${depName}: computed "${range}", packages/agentkit/package.json ` +
        `has ${declared[depName] ? `"${declared[depName]}"` : "(missing)"}`,
    );
  }
}
for (const depName of Object.keys(declared)) {
  if (!union.has(depName)) {
    driftMessages.push(
      `  ${depName}: declared in packages/agentkit/package.json ` +
        `("${declared[depName]}") but no source package depends on it anymore`,
    );
  }
}
if (driftMessages.length > 0) {
  fail(
    `packages/agentkit/package.json's "dependencies" is out of sync with ` +
      "the union of the ten source packages' runtime dependencies:\n" +
      `${driftMessages.join("\n")}\n` +
      "Update packages/agentkit/package.json to match.",
  );
}

// ---------------------------------------------------------------------------
// 3. Wipe and recreate the umbrella dist
// ---------------------------------------------------------------------------

rmSync(UMBRELLA_DIST, { recursive: true, force: true });
mkdirSync(UMBRELLA_DIST, { recursive: true });

// ---------------------------------------------------------------------------
// 4. Copy + rewrite
// ---------------------------------------------------------------------------

const SOURCE_MAP_COMMENT = /^\/\/# sourceMappingURL=.*$/gm;

/** POSIX-relative import target, always ending in `/index.js`, always starting `./` or `../`. */
function relativeIndexImport(fromFileDir, targetSubpath) {
  const targetFile = join(UMBRELLA_DIST, targetSubpath, "index.js");
  let rel = relative(fromFileDir, targetFile).split(sep).join("/");
  if (!rel.startsWith(".")) rel = `./${rel}`;
  return rel;
}

/**
 * Rewrites every `@agentkit/<pkg>` module specifier to its relative dist
 * path, then collapses any leftover "@agentkit/" text (e.g. inside a
 * comment) so the old scope cannot survive anywhere in the copied file.
 *
 * Between the two, anything still SPECIFIER-SHAPED is a hard failure — see
 * `./umbrella-specifiers.mjs`. Collapsing one of those would produce a bare
 * `agentkit/...` import of a module that does not exist, and would do it
 * invisibly: the "no @agentkit/ remains" check at the end would pass, because
 * the collapse is what removed the evidence.
 */
function rewriteContent(content, fileDir, sourcePath) {
  const withoutMaps = content.replace(SOURCE_MAP_COMMENT, "");
  const withRewrittenSpecifiers = withoutMaps.replace(
    AGENTKIT_SPECIFIER,
    (match, quote, pkgName) => {
      if (!SUBPATHS.includes(pkgName)) return match;
      return `${quote}${relativeIndexImport(fileDir, pkgName)}${quote}`;
    },
  );
  const residual = findResidualSpecifiers(withRewrittenSpecifiers);
  if (residual.length > 0) {
    fail(
      `${relative(ROOT, sourcePath)} still holds @agentkit/ module specifiers ` +
        "the rewrite did not recognise (a subpath import, a template " +
        "literal, or a package missing from SUBPATHS). Collapsing them would " +
        "ship a broken bare import that nothing downstream can detect:\n" +
        residual.map((r) => `  line ${r.line}: ${r.text}`).join("\n"),
    );
  }
  return withRewrittenSpecifiers.replace(/@agentkit\//g, "agentkit/");
}

function copyDistTree(srcDir, dstDir) {
  for (const entry of readdirSync(srcDir, { withFileTypes: true })) {
    const srcPath = join(srcDir, entry.name);
    const dstPath = join(dstDir, entry.name);
    if (entry.isDirectory()) {
      mkdirSync(dstPath, { recursive: true });
      copyDistTree(srcPath, dstPath);
      continue;
    }
    if (entry.name.endsWith(".map")) continue; // dropped by design
    if (entry.name.endsWith(".js") || entry.name.endsWith(".d.ts")) {
      const content = readFileSync(srcPath, "utf8");
      writeFileSync(
        dstPath,
        rewriteContent(content, dirname(dstPath), srcPath),
      );
    } else {
      // Non-JS runtime assets (e.g. testing's golden-trace JSON) — copy as-is.
      cpSync(srcPath, dstPath);
    }
  }
}

for (const name of SUBPATHS) {
  const srcDir = join(ROOT, "packages", name, "dist");
  const dstDir = join(UMBRELLA_DIST, name);
  mkdirSync(dstDir, { recursive: true });
  copyDistTree(srcDir, dstDir);
}

// ---------------------------------------------------------------------------
// 5. Verify
// ---------------------------------------------------------------------------

function collectFiles(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) collectFiles(path, out);
    else out.push(path);
  }
  return out;
}

const allFiles = collectFiles(UMBRELLA_DIST);

const leftovers = allFiles.filter((file) =>
  readFileSync(file, "utf8").includes("@agentkit/"),
);
if (leftovers.length > 0) {
  fail(
    '"@agentkit/" still appears in these built files after rewriting:\n' +
      leftovers.map((f) => `  ${relative(ROOT, f)}`).join("\n"),
  );
}

const missingExports = [];
for (const [subpath, target] of Object.entries(umbrellaPkg.exports ?? {})) {
  for (const kind of ["types", "import"]) {
    const rel = target[kind];
    if (typeof rel !== "string") {
      missingExports.push(`${subpath}: no "${kind}" entry`);
      continue;
    }
    const abs = join(UMBRELLA_DIR, rel.replace(/^\.\//, ""));
    if (!existsSync(abs)) {
      missingExports.push(`${subpath}: "${kind}" -> ${rel} does not exist`);
    }
  }
}
if (missingExports.length > 0) {
  fail(
    'packages/agentkit/package.json "exports" entries do not resolve:\n' +
      missingExports.map((m) => `  ${m}`).join("\n"),
  );
}

console.log(
  `build-umbrella: packages/agentkit/dist assembled from ${SUBPATHS.length} ` +
    `packages (${allFiles.length} files).`,
);
