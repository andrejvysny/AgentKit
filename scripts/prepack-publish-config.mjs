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
 */
import { existsSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";

const BACKUP_PATH = "package.json.publishconfig-backup";
const PKG_PATH = "package.json";
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
