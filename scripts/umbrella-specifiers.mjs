/**
 * The `@agentkit/*` rewriting `build-umbrella.mjs` does, and the guard that
 * catches what it missed.
 *
 * Split out of the build script because the guard is the interesting half and a
 * self-executing script cannot be tested: see `scripts/tests/umbrella-specifiers.test.ts`.
 *
 * WHY THE GUARD EXISTS. Step 4 of the build rewrites every `@agentkit/<pkg>`
 * MODULE SPECIFIER to a relative path into the umbrella dist, then collapses
 * any leftover `@agentkit/` TEXT to `agentkit/` so the old scope survives
 * nowhere in the shipped package. That collapse is right for a doc comment and
 * catastrophic for a specifier the rewrite did not recognise: a subpath import
 * (`"@agentkit/host/foo"`) or a computed one (`` `@agentkit/${name}` ``) is
 * silently turned into a bare `agentkit/...` import of a module that does not
 * exist, and the final "no `@agentkit/` remains" check passes — the evidence
 * was destroyed by the thing being checked. A build that ships a broken import
 * quietly is worse than one that fails, so a residual specifier is fatal.
 */

import { NATIVE_PLATFORM_TARGETS } from "../rust/scripts/native-platform-targets.mjs";

/**
 * `@agentkit/native-<platform-key>` are EXTERNAL npm packages (the prebuilt
 * addons), not umbrella subpaths: they keep their scoped name in the shipped
 * dist. Matches a literal key or the `native-${` head of a template literal
 * that builds one (the wrapper loader's version guard does).
 */
const PLATFORM_KEYS = NATIVE_PLATFORM_TARGETS.map((t) => t.key).join("|");
const PLATFORM_SCOPE = new RegExp(
  `@agentkit/native-(?:(?:${PLATFORM_KEYS})(?![\\w-])|\\$\\{)`,
  "g",
);
const KEPT = "\u0000kept\u0000";

/** Collapses leftover `@agentkit/` text to `agentkit/`, sparing platform packages. */
export function collapseAgentkitScope(content) {
  return content
    .replace(PLATFORM_SCOPE, (match) => match.replace("@", KEPT))
    .replace(/@agentkit\//g, "agentkit/")
    .replaceAll(KEPT, "@");
}

/** True when `@agentkit/` remains in `content` outside the platform packages. */
export function hasUnexpectedScope(content) {
  return content.replace(PLATFORM_SCOPE, "").includes("@agentkit/");
}

/** Quote-delimited `@agentkit/<name>` — the shape the rewrite understands. */
export const AGENTKIT_SPECIFIER = /(["'])@agentkit\/([a-zA-Z0-9_-]+)\1/g;

/**
 * What must NOT remain after the rewrite, and nothing else.
 *
 * - `"@agentkit/` / `'@agentkit/` — a quoted specifier the rewrite left alone
 *   (unknown package, or a subpath: `"@agentkit/host/internal.js"`).
 * - `@agentkit/${` — a specifier built in a template literal.
 *
 * A BACKTICK alone is deliberately NOT a signal: this codebase writes
 * `` `@agentkit/host` `` inside doc comments constantly, and those are exactly
 * the prose mentions the collapse is for.
 */
const RESIDUAL_SPECIFIER = /(?:["']@agentkit\/)|(?:@agentkit\/\$\{)/;

/**
 * Every line of `content` still holding a specifier-shaped `@agentkit/`.
 *
 * Returns `[{ line, text }]` with 1-based line numbers, so a failure can name
 * the file AND the line instead of leaving the reader to grep a built dist.
 */
export function findResidualSpecifiers(content) {
  const found = [];
  const lines = content.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const text = lines[index];
    if (RESIDUAL_SPECIFIER.test(text.replace(PLATFORM_SCOPE, ""))) {
      found.push({ line: index + 1, text: text.trim() });
    }
  }
  return found;
}
