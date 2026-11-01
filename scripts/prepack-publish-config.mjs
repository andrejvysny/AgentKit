#!/usr/bin/env node
/**
 * Works around a real npm limitation: `publishConfig.exports` (and
 * `.main`/`.types`) is NOT applied by plain `npm pack` or `npm publish` —
 * that override is pnpm-specific (confirmed against npm 10.9.8 by packing a
 * package here and inspecting the tarball's `package.json`; also tracked
 * upstream as https://github.com/npm/cli/issues/7586, still open). `bun
 * publish` has the same gap when it packs the package itself.
 *
 * Every publishable package's `exports` deliberately points at TypeScript
 * SOURCE during development (`./src/index.ts`) — see `scripts/node-smoke.mjs`
 * — with the dist-pointing shape recorded under `publishConfig` for "what a
 * published install should resolve to". Since npm won't apply that swap on
 * its own, packing the package.json AS-IS would ship a `.ts` entry point
 * that plain Node cannot load — exactly the packaging bug
 * `scripts/pack-smoke.mjs` exists to catch.
 *
 * Every publishable package's `package.json` wires this in as
 *
 *   "prepack":  "node ../../scripts/prepack-publish-config.mjs apply"
 *   "postpack": "node ../../scripts/prepack-publish-config.mjs restore"
 *
 * `npm pack`/`npm publish`/`bun publish` all run `prepack` immediately before
 * building the tarball and `postpack` immediately after — see npm's
 * lifecycle-scripts docs — so the merge is applied to the exact package.json
 * that gets tarred, then reverted, leaving the working tree exactly as it was
 * before the pack. `bun install` does NOT run prepack/postpack (only
 * {pre,post}install and {pre,post}prepare), so this never fires on a normal
 * install.
 *
 * Run from the package directory being packed (that is where npm invokes
 * lifecycle scripts) — this script always operates on `./package.json`.
 *
 * `apply` additionally pins every `workspace:` protocol range to a concrete
 * version. The workspace protocol is a link-time fiction of the monorepo:
 * npm cannot resolve it for an installed consumer (`EUNSUPPORTEDPROTOCOL`),
 * and a packed manifest that keeps one is a hard gate failure —
 * `rust/scripts/check-native-package.mjs` and `scripts/qualify-package.mjs`
 * both refuse any tarball whose package.json still contains `workspace:`.
 * Resolution walks the sibling package (`../<name>` etc.) for its real
 * version and falls back to the packed package's own version, which is
 * correct under this repo's lockstep versioning (every workspace ships the
 * same `0.1.0-dev` until `snapshot-release` bumps them together) and is what
 * lets a staged single-package copy — where no sibling exists, as in
 * `check-native-package.mjs`'s staging directory — pack cleanly.
 */
import { existsSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const BACKUP_PATH = "package.json.publishconfig-backup";
const PKG_PATH = "package.json";

function fail(message) {
  console.error(`${PKG_PATH}: ${message}`);
  process.exit(1);
}
// Fields npm's own historical (pre-`exports`) publishConfig override
// supports for `main`/`bin`, extended here to the fields this repo actually
// overrides. Anything else under `publishConfig` (e.g. registry/tag/access,
// real npm config) is left alone — this script only touches these.
const OVERRIDABLE_FIELDS = [
  "main",
  "types",
  "module",
  "browser",
  "bin",
  "exports",
];
const DEPENDENCY_FIELDS = [
  "dependencies",
  "devDependencies",
  "peerDependencies",
  "optionalDependencies",
];

/** First real version found for `name` among the ancestors of `cwd`, using
 * the repo layout (`<ancestor>/packages/<name>`, `<ancestor>/examples/<name>`,
 * `<ancestor>/<name>`); `undefined` when no sibling is on disk. */
function siblingVersion(name, cwd) {
  const bare = name.startsWith("@") ? name.slice(name.indexOf("/") + 1) : name;
  let dir = cwd;
  for (;;) {
    for (const candidate of [
      join(dir, "packages", bare),
      join(dir, "examples", bare),
      join(dir, bare),
    ]) {
      const manifest = join(candidate, "package.json");
      if (existsSync(manifest)) {
        try {
          const version = JSON.parse(readFileSync(manifest, "utf8")).version;
          if (typeof version === "string") return version;
        } catch {
          /* not a readable manifest — keep walking */
        }
      }
    }
    const parent = dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
}

/** Replace every `workspace:` range in the dependency fields with a concrete
 * pin (`workspace:*` → exact, `workspace:^`/`workspace:~` → prefixed), so a
 * packed manifest resolves for plain npm/bun. Aliases and exotic specs fail
 * loudly — a pack must never ship a range no installer can parse. */
function pinWorkspaceRanges(pkg, cwd) {
  let pinned = 0;
  for (const field of DEPENDENCY_FIELDS) {
    const deps = pkg[field];
    if (!deps || typeof deps !== "object") continue;
    for (const [name, range] of Object.entries(deps)) {
      if (typeof range !== "string" || !range.startsWith("workspace:"))
        continue;
      const spec = range.slice("workspace:".length);
      if (spec !== "*" && spec !== "^" && spec !== "~")
        fail(
          `${field}.${name} uses ${JSON.stringify(range)}; only workspace:*, ` +
            "workspace:^ and workspace:~ are packable — name the alias or use a plain range.",
        );
      const version = siblingVersion(name, cwd) ?? pkg.version;
      if (typeof version !== "string" || version.length === 0)
        fail(
          `cannot pin ${field}.${name}@${range}: no sibling manifest found ` +
            `above ${cwd} and the packed package has no version of its own`,
        );
      deps[name] = spec === "*" ? version : `${spec}${version}`;
      pinned += 1;
    }
  }
  return pinned;
}

function apply() {
  if (existsSync(BACKUP_PATH)) {
    console.error(
      `${BACKUP_PATH} already exists in ${process.cwd()} — a previous ` +
        "apply did not restore cleanly (an interrupted pack?). Refusing to " +
        "overwrite a backup that might be the real original; resolve by " +
        "hand (diff the backup against package.json, then delete the " +
        "backup) before packing again.",
    );
    process.exit(1);
  }
  const raw = readFileSync(PKG_PATH, "utf8");
  writeFileSync(BACKUP_PATH, raw);
  const pkg = JSON.parse(raw);
  const overrides = pkg.publishConfig ?? {};
  for (const field of OVERRIDABLE_FIELDS) {
    if (field in overrides) pkg[field] = overrides[field];
  }
  const pinned = pinWorkspaceRanges(pkg, process.cwd());
  if (pinned > 0)
    process.stdout.write(
      `prepack: pinned ${pinned} workspace: dependency range(s) in ${PKG_PATH}\n`,
    );
  writeFileSync(PKG_PATH, `${JSON.stringify(pkg, null, 2)}\n`);
}

function restore() {
  if (!existsSync(BACKUP_PATH)) return; // nothing to undo — apply never ran
  writeFileSync(PKG_PATH, readFileSync(BACKUP_PATH));
  unlinkSync(BACKUP_PATH);
}

const mode = process.argv[2];
if (mode === "apply") apply();
else if (mode === "restore") restore();
else {
  console.error("usage: prepack-publish-config.mjs <apply|restore>");
  process.exit(1);
}
