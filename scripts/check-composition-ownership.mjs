#!/usr/bin/env node
/**
 * Structural composition-ownership gate (review 2026-11-01 §6.6).
 *
 * The migration's final structural invariant: a SHIPPED COMPOSITION — an
 * example application or a qualification driver that stands in for one —
 * must execute the Rust backend through `@agentkit/native` bindings. It may
 * import bindings, public types, browser clients, contracts, and React, but
 * NOT the frozen TypeScript backend implementations. Without this gate the
 * parity ledger (`docs/rust/parity.md`) is enforced by nothing mechanical:
 * a regression reintroducing the TS `TurnRunner` or the TS coding pack
 * would pass every other check.
 *
 * Rules (value imports; `import type` and inline `type` specifiers are
 * always allowed — public types are part of the frozen contract):
 *  1. A backend package (BACKEND below) may not be imported from a scanned
 *     composition file, EXCEPT for the exact per-file symbol allowlist
 *     below, each entry carrying its documented reason.
 *  2. Namespace/default imports of a backend package are a violation
 *     everywhere except the compile-time API-surface probe.
 *  3. HARD_BANNED symbols are violations even inside allowlisted files —
 *     the named execution engines stay out of compositions, permanently.
 *
 * Usage: node scripts/check-composition-ownership.mjs [--json]
 * Exit 0 when the tree is clean, 1 with a per-violation report otherwise.
 */
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join, posix, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));

/** Backend implementation packages (the frozen TS stack). `agentkit/...`
 * umbrella subpaths normalize through SUBPATH_MAP. */
const BACKEND = new Set([
  "@agentkit/host",
  "@agentkit/core",
  "@agentkit/adapters-memory",
  "@agentkit/adapters-sqlite",
  "@agentkit/runner-local",
  "@agentkit/mcp-client",
  "@agentkit/transport-http",
]);
const UMBRELLA = "agentkit/";
/** Umbrella subpaths that address the same backend implementations.
 * `adapters-sqlite-node` is the Node entry of `adapters-sqlite`. */
const SUBPATH_MAP = {
  "adapters-sqlite-node": "@agentkit/adapters-sqlite",
};

/** Symbols no shipped composition may EVER execute-import. They name the
 * frozen TS execution engines; the native bindings replace them. */
export const HARD_BANNED = new Set([
  "createCodingToolPack",
  "createLockDirWorkspaceCoordinator",
  "TurnRunner",
  "TaskService",
  "SingleProcessTaskRunner",
  "createRestHandler",
]);

/** Per-file allowlist for VALUE imports that remain deliberately TS:
 * configuration metadata and acceptance harnesses for legs the native
 * binding has not claimed yet (each reason mirrors docs/rust/parity.md). */
const ALLOWLIST = {
  "examples/coding-agent/src/app.ts": {
    "@agentkit/host": {
      symbols: ["defaultProfileRegistry", "readSessionDescriptor"],
      reason:
        "profile/descriptor configuration helpers — documented carve-out (docs/rust/parity.md), not an execution engine",
    },
  },
  "examples/coding-agent/src/main.ts": {
    "@agentkit/host": {
      symbols: ["defaultProfileRegistry", "REASONING_EFFORTS"],
      reason: "same carve-out as app.ts; REASONING_EFFORTS is a static enum",
    },
  },
  "examples/coding-agent/src/commands.ts": {
    "@agentkit/host": {
      symbols: ["REASONING_EFFORTS"],
      reason: "static effort enum for CLI argument parsing, no runtime backend",
    },
  },
  "examples/desktop-host/src/wiring.ts": {
    "@agentkit/core": {
      symbols: ["getPresetByKind"],
      reason:
        "provider preset metadata (static config table); the provider transport itself runs in Rust",
    },
  },
  "examples/desktop-host/src/managed-connector-acceptance.ts": {
    "@agentkit/mcp-client": {
      symbols: ["ConnectionService", "McpError", "referenceConnectors"],
      reason:
        "OAuth connector acceptance harness for the refused-not-wired leg (docs/rust/native-binding-milestone.md, Open gaps)",
    },
  },
  "scripts/qualification/migration-seed.mjs": {
    "@agentkit/adapters-sqlite": {
      symbols: ["NodeSqliteAssistantStore"],
      reason:
        "seeds the LEGACY TS store so the native store's migration path is proven against the real old writer",
    },
  },
  "scripts/qualification/proposals-smoke.mjs": {
    "@agentkit/host": {
      symbols: [
        "createProposalBuilderTool",
        "ProposalService",
        "SessionWritePolicy",
        "defaultClock",
        "defaultIds",
      ],
      reason:
        "frozen TS proposal-engine DRIVER over the native store port — a store-contract conformance witness (same differential role as packages/testing store-conformance); the TS engine is kept until its replacement is proven",
    },
  },
  "scripts/qualification/responses-smoke.mjs": {
    "@agentkit/adapters-sqlite": {
      symbols: ["NodeSqliteAssistantStore"],
      reason: "legacy durable-replay witness over a pre-migration database",
    },
  },
};

/** Files exempt from the namespace-import rule (compile-time only probes). */
const NAMESPACE_ALLOW = {
  "scripts/qualification/types.ts":
    "compile-time API-surface probe (tsc consumption check); no runtime execution",
};

/** Composition roots scanned; tests/fixtures never ship. */
const SCAN_ROOTS = ["examples", "scripts/qualification"];
const SCAN_EXTENSIONS = new Set([".ts", ".tsx", ".mts", ".js", ".mjs"]);
const SKIP_DIR = new Set(["node_modules", "dist", "tests", "fixtures", "test"]);

function listFiles(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const st = statSync(full);
    if (st.isDirectory()) {
      if (SKIP_DIR.has(entry)) continue;
      out.push(...listFiles(full));
    } else if (SCAN_EXTENSIONS.has(entry.slice(entry.lastIndexOf(".")))) {
      if (/\.test\.[tm]sx?$/.test(entry)) continue;
      out.push(full);
    }
  }
  return out;
}

const IMPORT_RE =
  /^[ \t]*import\s+(type\s+)?([\s\S]*?)\s*from\s*["']([^"']+)["']/gm;
const EXPORT_FROM_RE =
  /^[ \t]*export\s+(type\s+)?([\s\S]*?)\s*from\s*["']([^"']+)["']/gm;
const REQUIRE_RE = /\brequire\(\s*["']([^"']+)["']\s*\)/g;

function normalizeSpecifier(spec) {
  if (BACKEND.has(spec)) return spec;
  if (spec.startsWith(UMBRELLA)) {
    const sub = spec.slice(UMBRELLA.length).split("/")[0];
    const mapped = SUBPATH_MAP[sub] ?? `@agentkit/${sub}`;
    return BACKEND.has(mapped) ? mapped : null;
  }
  return null;
}

/** Parse one source file into {spec, clause, typeOnly} import records. */
function execAll(re, source) {
  const matches = [];
  re.lastIndex = 0;
  for (;;) {
    const match = re.exec(source);
    if (match === null) break;
    matches.push(match);
  }
  return matches;
}

export function parseImports(source) {
  const found = [];
  for (const [re, kind] of [
    [IMPORT_RE, "import"],
    [EXPORT_FROM_RE, "export"],
  ]) {
    for (const match of execAll(re, source)) {
      const wholeType = match[1] !== undefined;
      const clause = (match[2] ?? "").trim();
      found.push({ spec: match[3], clause, typeOnly: wholeType, kind });
    }
  }
  for (const match of execAll(REQUIRE_RE, source)) {
    found.push({
      spec: match[1],
      clause: null,
      typeOnly: false,
      kind: "require",
    });
  }
  return found;
}

/** Hard-banned engine symbols named in an import/export clause. These are
 * tombstones: refused wherever they appear, including specs of packages
 * that no longer ship — the names must never execute in a composition. */
function bannedSymbolsIn(record) {
  if (record.typeOnly || record.clause === null) return [];
  if (!record.clause.startsWith("{")) return [];
  return record.clause
    .replace(/^\{|\}$/g, "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "" && !/^type\s+/.test(s))
    .map((raw) =>
      raw
        .replace(/^\{\s*/, "")
        .split(/\s+as\s+/)[0]
        .trim(),
    )
    .filter((symbol) => HARD_BANNED.has(symbol));
}

/** Classify one import record against the rules. Returns an array of
 * violation strings (empty = clean). `reasonFor(spec)` resolves the
 * allowlist entry for this file. */
function checkImport(record, reasonFor) {
  const banned = bannedSymbolsIn(record);
  if (banned.length > 0) {
    return banned.map(
      (symbol) =>
        `hard-banned backend execution symbol \`${symbol}\` imported from \`${record.spec}\``,
    );
  }
  const backend = normalizeSpecifier(record.spec);
  if (!backend) return [];
  if (record.kind === "export") {
    // Re-exports carry no executable code of their own, but re-exporting a
    // backend module from a composition publishes it as the composition's
    // surface — only allowed for pure type re-exports.
    if (record.typeOnly) return [];
    const allowed = reasonFor(backend);
    return allowed
      ? []
      : [
          `re-exports \`${record.spec}\` (a backend implementation) from a shipped composition`,
        ];
  }
  if (record.clause === null) {
    return [`bare/require import of backend package \`${record.spec}\``];
  }
  if (!record.clause.startsWith("{")) {
    // default or namespace import: whole-package value dependency.
    return [
      `whole-package (\`${record.clause}\`) import of backend \`${record.spec}\``,
    ];
  }
  const violations = [];
  for (const raw of record.clause
    .replace(/^\{|\}$/g, "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "")) {
    const inlineType = /^type\s+/.test(raw);
    if (record.typeOnly || inlineType) continue;
    const symbol = raw
      .replace(/^\{\s*/, "")
      .split(/\s+as\s+/)[0]
      .trim();
    const allowed = reasonFor(backend);
    if (!allowed || !allowed.symbols.includes(symbol)) {
      violations.push(
        `value import \`${symbol}\` from backend \`${record.spec}\` is not allowlisted for this file`,
      );
    }
  }
  return violations;
}

/** Scan one source string; `fileRel` keys the allowlists. */
export function scanSource(fileRel, source) {
  const findings = [];
  const fileAllow = ALLOWLIST[fileRel] ?? {};
  const namespaceAllowed = NAMESPACE_ALLOW[fileRel];
  for (const record of parseImports(source)) {
    const banned = bannedSymbolsIn(record);
    if (banned.length > 0) {
      // Tombstone: flagged before the BACKEND prefilter, so the engine
      // names stay refused even after their package no longer ships.
      for (const symbol of banned) {
        findings.push({
          file: fileRel,
          spec: record.spec,
          violation: `hard-banned backend execution symbol \`${symbol}\` imported from \`${record.spec}\``,
        });
      }
      continue;
    }
    const backend = normalizeSpecifier(record.spec);
    if (!backend) continue;
    if (
      namespaceAllowed &&
      record.kind === "import" &&
      record.clause &&
      !record.clause.startsWith("{")
    ) {
      continue; // documented compile-time probe
    }
    const reasonFor = (spec) => fileAllow[spec] ?? null;
    for (const violation of checkImport(record, reasonFor)) {
      findings.push({ file: fileRel, spec: record.spec, violation });
    }
  }
  return findings;
}

export function scanTree() {
  const findings = [];
  let files = 0;
  for (const relRoot of SCAN_ROOTS) {
    const dir = join(root, relRoot);
    if (!existsSync(dir)) continue;
    for (const file of listFiles(dir)) {
      files += 1;
      const fileRel = relative(root, file).split("\\").join("/");
      findings.push(...scanSource(fileRel, readFileSync(file, "utf8")));
    }
  }
  return { findings, files };
}

function main() {
  const { findings, files } = scanTree();
  if (process.argv.includes("--json")) {
    process.stdout.write(`${JSON.stringify({ files, findings }, null, 2)}\n`);
  } else if (findings.length === 0) {
    process.stdout.write(
      `check-composition-ownership: ${files} composition files scanned, 0 backend-implementation imports (bindings, public types, clients and the documented allowlist only)\n`,
    );
  } else {
    for (const f of findings) {
      process.stdout.write(`VIOLATION ${f.file}: ${f.violation}\n`);
    }
    process.stdout.write(
      `check-composition-ownership: ${findings.length} violation(s) — shipped compositions must execute the Rust backend through @agentkit/native; see docs/rust/parity.md\n`,
    );
  }
  process.exit(findings.length === 0 ? 0 : 1);
}

if (
  process.argv[1] &&
  posix.basename(fileURLToPath(import.meta.url)) ===
    posix.basename(process.argv[1])
) {
  main();
}
