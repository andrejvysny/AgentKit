#!/usr/bin/env node
// Enforce docs/adr/0021-layered-crate-architecture.md over `cargo metadata`.
// Existing violations live in arch-allowlist.json, which may only shrink.
import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const rustRoot = resolve(scriptDir, "..");
const allowlistPath = join(scriptDir, "arch-allowlist.json");
const update = process.argv.includes("--update-allowlist");

const LAYERS = {
  "agentkit-kernel": 1,
  "agentkit-contracts": 1,
  "agentkit-core": 2,
  "agentkit-governance": 2,
  "agentkit-engine": 3,
  "agentkit-sessions": 3,
  "agentkit-chat": 3,
  "agentkit-agents": 3,
  "agentkit-memory": 3,
  "agentkit-automation": 3,
  "agentkit-remote": 3,
  "agentkit-host": 3, // legacy, transitional
  "agentkit-store": 4,
  "agentkit-store-sqlite": 4,
  "agentkit-store-legacy-import": 4,
  "agentkit-providers": 4,
  "agentkit-toolpacks-coding": 4,
  "agentkit-lsp": 4,
  "agentkit-mcp": 4,
  "agentkit-connections": 4,
  "agentkit-sys": 4,
  "agentkit-runtime": 5,
  "agentkit-napi": 6,
  "agentkit-http": 6,
  "agentkit-client": 6,
  "agentkit-rust": 6,
};
const TESTKIT = "agentkit-testkit";
const BANNED_INNER = new Set([
  "axum",
  "napi",
  "napi-derive",
  "rusqlite",
  "reqwest",
  "rmcp",
  "keyring",
]);
// R2: layer-3 package -> layer-3 packages it may depend on.
const CONTEXT_DEPS = {
  "agentkit-engine": [],
  "agentkit-sessions": ["agentkit-engine"],
  "agentkit-chat": ["agentkit-engine", "agentkit-sessions"],
  "agentkit-agents": ["agentkit-engine", "agentkit-sessions", "agentkit-chat"],
  "agentkit-memory": ["agentkit-engine", "agentkit-sessions"],
  "agentkit-automation": ["agentkit-engine", "agentkit-sessions"],
  "agentkit-remote": ["agentkit-engine", "agentkit-sessions"],
  "agentkit-host": [
    "agentkit-engine",
    "agentkit-sessions",
    "agentkit-chat",
    "agentkit-agents",
    "agentkit-memory",
    "agentkit-automation",
    "agentkit-remote",
  ],
};
const SKIP_DIRS = new Set(["tests", "benches", "examples", "target"]);
const STATIC_RE =
  /^\s*(pub(\([^)]*\))?\s+)?static\s+[A-Z_0-9]+\s*:\s*(std::sync::)?(Mutex|RwLock|LazyLock|OnceLock)</;

function fail(message) {
  process.stderr.write(`check-arch: ${message}\n`);
  process.exit(1);
}

function loadMetadata() {
  const result = spawnSync(
    "cargo",
    [
      "metadata",
      "--format-version",
      "1",
      "--locked",
      "--manifest-path",
      join(rustRoot, "Cargo.toml"),
    ],
    { encoding: "utf8", maxBuffer: 256 * 1024 * 1024 },
  );
  if (result.error) throw result.error;
  if (result.status !== 0) fail(`cargo metadata failed\n${result.stderr}`);
  return JSON.parse(result.stdout);
}

function isExample(pkg) {
  const path = pkg.manifest_path.split(sep).join("/");
  return path.includes("/examples/");
}

function graphViolations(metadata, crates) {
  const violations = new Set();
  const byId = new Map(metadata.packages.map((pkg) => [pkg.id, pkg]));
  const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
  for (const crate of crates) {
    const from = crate.name;
    const layer = LAYERS[from];
    for (const dep of nodes.get(crate.id).deps) {
      if (!dep.dep_kinds.some((kind) => kind.kind !== "dev")) continue;
      const to = byId.get(dep.pkg).name;
      if (to === TESTKIT) violations.add(`R3 ${from} -> ${to}`);
      if (layer <= 3 && BANNED_INNER.has(to))
        violations.add(`R1 ${from} -> ${to}`);
      const toLayer = LAYERS[to];
      if (toLayer === undefined) continue; // external crate
      if (toLayer > layer) violations.add(`R1b ${from} -> ${to}`);
      else if (layer === 3 && toLayer === 3) {
        if (!(CONTEXT_DEPS[from] ?? []).includes(to))
          violations.add(`R2 ${from} -> ${to}`);
      }
    }
  }
  return violations;
}

function* sourceFiles(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) yield* sourceFiles(path);
    } else if (
      entry.name.endsWith(".rs") &&
      !entry.name.endsWith("_tests.rs")
    ) {
      yield path;
    }
  }
}

function sourceViolations(crates) {
  const violations = new Set();
  for (const crate of crates) {
    const root = dirname(crate.manifest_path);
    if (!root.startsWith(join(rustRoot, "crates") + sep)) continue;
    const src = join(root, "src");
    for (const file of sourceFiles(src)) {
      const rel = relative(root, file).split(sep).join("/");
      const lines = readFileSync(file, "utf8").split("\n");
      for (const line of lines) {
        const text = line.trim();
        if (text.startsWith("//")) continue;
        const prefix = `R5 ${crate.name} ${rel}`;
        const glob = text.match(/^pub use (\w+)::\*;/);
        if (rel === "src/lib.rs" && glob)
          violations.add(`${prefix} pub use ${glob[1]}::*`);
        const path = text.match(/#\[path\s*=\s*"([^"]*)"/);
        if (path) violations.add(`${prefix} #[path] ${path[1]}`);
        if (crate.name === "agentkit-sys") continue;
        if (/^(pub(\([^)]*\))?\s+)?static\s+mut\s/.test(text)) {
          const name = text.match(/static\s+mut\s+(\w+)/)?.[1] ?? "?";
          violations.add(`${prefix} static mut ${name}`);
        } else if (STATIC_RE.test(line)) {
          violations.add(`${prefix} static ${text.match(/static\s+(\w+)/)[1]}`);
        }
      }
    }
  }
  return violations;
}

const metadata = loadMetadata();
const members = new Set(metadata.workspace_members);
const crates = metadata.packages.filter(
  (pkg) => members.has(pkg.id) && !isExample(pkg),
);
const unclassified = crates.filter(
  (pkg) => LAYERS[pkg.name] === undefined && pkg.name !== TESTKIT,
);
if (unclassified.length > 0)
  fail(
    unclassified
      .map((pkg) => `unclassified crate ${pkg.name}; add it to LAYERS`)
      .join("\n"),
  );
const checked = crates.filter((pkg) => pkg.name !== TESTKIT);

const found = new Set([
  ...graphViolations(metadata, checked),
  ...sourceViolations(checked),
]);

if (update) {
  const comment =
    "Existing architecture violations (see docs/adr/0021). Only shrinks; remove entries as migration phases resolve them.";
  writeFileSync(
    allowlistPath,
    `${JSON.stringify({ comment, violations: [...found].sort() }, null, 2)}\n`,
  );
  process.stdout.write(`check-arch: wrote ${found.size} violations\n`);
  process.exit(0);
}

const allowed = new Set(
  JSON.parse(readFileSync(allowlistPath, "utf8")).violations,
);
const added = [...found].filter((id) => !allowed.has(id)).sort();
const stale = [...allowed].filter((id) => !found.has(id)).sort();
if (added.length > 0 || stale.length > 0) {
  const lines = [
    ...added.map((id) => `violation: ${id}`),
    ...stale.map((id) => `stale allowlist entry, remove it: ${id}`),
  ];
  fail(`${lines.length} problem(s)\n  ${lines.join("\n  ")}`);
}
process.stdout.write(
  `check-arch: ok (${checked.length} crates, ${allowed.size} allowlisted violations)\n`,
);
