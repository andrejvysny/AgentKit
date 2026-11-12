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
  "agentkit-native": 2,
  "agentkit-engine": 3,
  "agentkit-sessions": 3,
  "agentkit-chat": 3,
  "agentkit-agents": 3,
  "agentkit-memory": 3,
  "agentkit-automation": 3,
  "agentkit-remote": 3,
  "agentkit-native-exec": 3,
  "agentkit-store": 4,
  "agentkit-providers": 4,
  "agentkit-toolpacks-coding": 4,
  "agentkit-lsp": 4,
  "agentkit-mcp": 4,
  "agentkit-connections": 4,
  "agentkit-sys": 4,
  "agentkit-native-broker": 4,
  "agentkit-claude": 4,
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
  "agentkit-native-exec": ["agentkit-engine", "agentkit-sessions"],
};
const SKIP_DIRS = new Set(["tests", "benches", "examples", "target"]);
// R5 (G1): a static is process-global mutable state when its type has
// interior mutability (mutexes, atomics, cells, sync primitives), when it
// is `static mut`, or when it sits in a `thread_local!` block. Immutable
// lazy constants — a `LazyLock<X>` whose X is one of the whitelisted
// value types — are not reported.
const STATIC_DECL_RE =
  /^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?([A-Z_0-9]+)\s*:/;
const MUTABLE_TYPE_RE =
  /Mutex|RwLock|Atomic[A-Za-z0-9]*|OnceLock|OnceCell|\bOnce\b|Notify|RefCell|\bCell<|Condvar|Barrier|Semaphore|DashMap/;
const LAZY_ALLOWED = new Set([
  "Regex",
  "regex::Regex",
  "Vec<Regex>",
  "Vec<regex::Regex>",
  "String",
  "Vec<String>",
  "Option<String>",
  "HashSet<&'staticstr>",
  "BTreeSet<&'staticstr>",
]);
const THREAD_LOCAL_RE = /\bthread_local\s*!\s*\{/;
const R12_LIMIT = 1000;

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
      // R11: among adapters, only napi may depend on the store.
      if (
        (layer === 4 || layer === 6) &&
        from !== "agentkit-napi" &&
        to === "agentkit-store"
      )
        violations.add(`R11 ${from} -> agentkit-store`);
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

// Type text of a declaration: after the first ':' up to the first '=' or
// ';'. A type never contains '='. If the declaration line has neither
// character, join up to five following lines first.
function typeText(lines, index) {
  let text = lines[index];
  for (let extra = 0; extra < 6; extra += 1) {
    const colon = text.indexOf(":");
    const cut = text.slice(colon + 1).search(/[=;]/);
    if (cut >= 0) return text.slice(colon + 1, colon + 1 + cut);
    if (extra === 5 || index + extra + 1 >= lines.length) break;
    text += "\n" + lines[index + extra + 1];
  }
  const colon = text.indexOf(":");
  return text.slice(colon + 1);
}

// One boolean per line: the line sits inside a `thread_local! { ... }`
// block, tracked by brace depth.
function threadLocalLines(lines) {
  const inside = new Array(lines.length).fill(false);
  const stack = []; // true per brace level that belongs to a thread_local block
  let depth = 0;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const tl = THREAD_LOCAL_RE.exec(line);
    // Depth at which the thread_local '{' opens (before this line's braces).
    let tlDepth = -1;
    if (tl) {
      let d = depth;
      for (let c = 0; c < tl.index + tl[0].length; c += 1) {
        if (line[c] === "{") d += 1;
        else if (line[c] === "}") d -= 1;
      }
      tlDepth = d - 1; // the block brace itself sits at depth d-1
    }
    let seenTlBrace = false;
    for (let c = 0; c < line.length; c += 1) {
      const ch = line[c];
      if (ch === "{") {
        const isTl = tlDepth >= 0 && !seenTlBrace && depth === tlDepth;
        if (isTl) seenTlBrace = true;
        stack.push(isTl);
        depth += 1;
      } else if (ch === "}") {
        stack.pop();
        depth -= 1;
      }
      if (stack.includes(true)) inside[i] = true;
    }
  }
  return inside;
}

function staticViolations(crateName, rel, lines, violations) {
  if (crateName === "agentkit-sys") return;
  const insideThreadLocal = threadLocalLines(lines);
  for (let i = 0; i < lines.length; i += 1) {
    const text = lines[i].trim();
    if (text.startsWith("//")) continue;
    const decl = STATIC_DECL_RE.exec(lines[i]);
    if (!decl) continue;
    const isMut = decl[3] !== undefined;
    const name = decl[4];
    const prefix = `R5 ${crateName} ${rel}`;
    if (isMut) {
      violations.add(`${prefix} static mut ${name}`);
      continue;
    }
    const type = typeText(lines, i);
    if (insideThreadLocal[i] || MUTABLE_TYPE_RE.test(type)) {
      violations.add(`${prefix} static ${name}`);
      continue;
    }
    let t = type.replace(/\s+/g, "");
    if (t.startsWith("std::sync::")) t = t.slice("std::sync::".length);
    if (t.startsWith("LazyLock<") && t.endsWith(">")) {
      const inner = t.slice("LazyLock<".length, -1);
      if (!LAZY_ALLOWED.has(inner)) violations.add(`${prefix} static ${name}`);
    }
  }
}

// R8: #[napi] exports that neither run under catch_unwind nor decode a
// derived object DTO. The measured count is part of the id, so the entry
// goes stale the moment an export gains catch_unwind — it can only lower.
function napiExportCount(crate) {
  const root = dirname(crate.manifest_path);
  const src = join(root, "src");
  let count = 0;
  for (const file of sourceFiles(src)) {
    for (const line of readFileSync(file, "utf8").split("\n")) {
      if (
        /^\s*#\[napi(\(|\])/.test(line) &&
        !line.includes("catch_unwind") &&
        !line.includes("object")
      )
        count += 1;
    }
  }
  return count;
}

function sourceViolations(crates) {
  const violations = new Set();
  for (const crate of crates) {
    const root = dirname(crate.manifest_path);
    if (!root.startsWith(join(rustRoot, "crates") + sep)) continue;
    const src = join(root, "src");
    const layer = LAYERS[crate.name];
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
      }
      staticViolations(crate.name, rel, lines, violations);
      // R12: file-size budget for layers 1-3 and 5.
      if ([1, 2, 3, 5].includes(layer) && lines.length > R12_LIMIT)
        violations.add(`R12 ${crate.name} ${rel}`);
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

const napiCrate = checked.find((pkg) => pkg.name === "agentkit-napi");
if (napiCrate) {
  const count = napiExportCount(napiCrate);
  if (count > 0)
    found.add(`R8 agentkit-napi exports-without-catch_unwind=${count}`);
}

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
// Per-rule report, counted by id prefix.
const RULES = ["R1b", "R5-glob", "R5-path", "R5-static", "R8", "R11", "R12"];
const counts = new Map(RULES.map((rule) => [rule, 0]));
for (const id of found) {
  let key;
  if (id.startsWith("R1b ")) key = "R1b";
  else if (id.startsWith("R5 ") && id.includes(" pub use ")) key = "R5-glob";
  else if (id.startsWith("R5 ") && id.includes(" #[path] ")) key = "R5-path";
  else if (id.startsWith("R5 ") && id.includes(" static ")) key = "R5-static";
  else if (id.startsWith("R8 ")) key = "R8";
  else if (id.startsWith("R11 ")) key = "R11";
  else if (id.startsWith("R12 ")) key = "R12";
  if (key) counts.set(key, counts.get(key) + 1);
}
const report = RULES.filter((rule) => counts.get(rule) > 0).map(
  (rule) => `${rule} ${counts.get(rule)}`,
);
process.stdout.write(
  `check-arch: ok (${checked.length} crates; ${report.join(", ")})\n`,
);
