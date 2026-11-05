#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  hostNativeTarget,
  NATIVE_PLATFORM_TARGETS,
} from "../rust/scripts/native-platform-targets.mjs";
import {
  captureSource,
  digest,
  freezeSource,
  overallStatus,
  prepareExecution,
  runGate,
  sameSource,
} from "./qualification/local-evidence.mjs";

const root = realpathSync(fileURLToPath(new URL("../", import.meta.url)));
const args = process.argv.slice(2);
const destination = args.shift();
if (!destination || destination.startsWith("--"))
  throw new Error(
    "Usage: node scripts/qualify-local.mjs NEW_OUTSIDE_DIRECTORY [--only gate,gate]",
  );
const requested = resolve(destination);
if (!existsSync(dirname(requested)))
  throw new Error("Evidence parent directory must already exist");
const output = join(
  realpathSync(dirname(requested)),
  requested.slice(dirname(requested).length + 1),
);
const inside = relative(root, output);
if (!isAbsolute(inside) && inside !== ".." && !inside.startsWith(`..${sep}`))
  throw new Error("Evidence directory must be outside checkout");
if (existsSync(output)) throw new Error("Evidence directory must be new");
let selected;
const runtimeInputs = {};
const runtimeSpecs = [
  ["node20", /^v20\./, "node"],
  ["node22", /^v22\./, "node"],
  ["node24", /^v24\./, "node"],
  ["bun-minimum", /^1\.3\.14$/, "bun"],
  ["bun-current-supported", /^1\.4\./, "bun"],
];
while (args.length) {
  const option = args.shift();
  const value = args.shift();
  if (!value) throw new Error(`Missing value: ${option}`);
  if (option === "--only") selected = new Set(value.split(","));
  else if (runtimeSpecs.some(([name]) => option === `--${name}`))
    runtimeInputs[option.slice(2)] = resolve(value);
  else throw new Error(`Unknown argument: ${option}`);
}
const target = hostNativeTarget();
const artifacts = join(output, "artifacts");
const manifest = join(artifacts, `artifact-manifest.${target.key}.json`);
const nativeEnv = { AGENTKIT_REQUIRE_NATIVE: "1" };
const gate = (id, command, commandArgs, options = {}) => ({
  id,
  command,
  args: commandArgs,
  cwd: root,
  ...options,
});
const gates = [
  gate("local-actions", process.execPath, ["scripts/check-local-actions.mjs"]),
  gate("install", "bun", ["install", "--frozen-lockfile"]),
  gate(
    "native-build",
    "bun",
    ["run", "--cwd", "packages/native", "build:native"],
    { requires: ["install"] },
  ),
  gate("rust-format", "cargo", ["fmt", "--all", "--check"], {
    cwd: join(root, "rust"),
  }),
  gate(
    "rust-tests",
    "cargo",
    ["test", "--workspace", "--all-features", "--locked"],
    { cwd: join(root, "rust"), requires: ["native-build"] },
  ),
  gate(
    "rust-clippy",
    "cargo",
    [
      "clippy",
      "--workspace",
      "--all-targets",
      "--all-features",
      "--locked",
      "--",
      "-D",
      "warnings",
    ],
    { cwd: join(root, "rust") },
  ),
  gate("rust-packages", process.execPath, ["rust/scripts/check-packages.mjs"]),
  gate(
    "native-tests",
    "bun",
    ["test", "packages/native", "--timeout", "60000"],
    { env: nativeEnv, requires: ["native-build"] },
  ),
  gate("build", "bun", ["run", "build"]),
  gate("umbrella-build", "bun", ["run", "build:umbrella"], {
    requires: ["build"],
  }),
  gate("api", "bun", ["run", "check:api"], { requires: ["build"] }),
  gate("composition", "bun", ["run", "check:composition"]),
  gate("differential", "bun", ["run", "check:parity"], {
    env: { ...nativeEnv, AGENTKIT_PARITY_OUTPUT: join(output, "parity") },
    requires: ["native-build"],
  }),
  gate("typescript", "bun", ["run", "typecheck"]),
  gate("lint", "bun", ["run", "lint"]),
  gate("bun-full", "bun", ["test", "--timeout", "60000"], {
    env: nativeEnv,
    requires: ["native-build"],
  }),
  gate(
    "native-package",
    process.execPath,
    ["rust/scripts/check-native-package.mjs"],
    { requires: ["native-build", "build"] },
  ),
  gate(
    "native-umbrella",
    process.execPath,
    ["rust/scripts/check-native-umbrella.mjs"],
    { requires: ["native-build", "umbrella-build"] },
  ),
  gate("cold-packages", process.execPath, [
    "scripts/qualify-cold-packages.mjs",
    join(output, "cold-packages"),
  ]),
  gate(
    "artifact-build",
    process.execPath,
    [
      "rust/scripts/build-native-release-artifact.mjs",
      "--target",
      target.triple,
      "--out",
      artifacts,
      ...(target.cross ? ["--allow-cross"] : []),
    ],
    { requires: ["build", "native-build"] },
  ),
  gate(
    "artifact-runtime",
    process.execPath,
    ["rust/scripts/check-native-artifact.mjs", "--manifest", manifest],
    { requires: ["artifact-build"] },
  ),
];
for (const [name, , family] of runtimeSpecs) {
  if (!runtimeInputs[name]) continue;
  gates.push(
    gate(
      `runtime-${name}`,
      process.execPath,
      ["rust/scripts/check-native-artifact.mjs", "--manifest", manifest],
      {
        requires: ["artifact-build"],
        runtimeFamily: family,
        runtimeName: name,
      },
    ),
  );
}
for (const name of selected ?? [])
  if (!gates.some(({ id }) => id === name))
    throw new Error(`Unknown gate: ${name}`);
mkdirSync(output, { recursive: true });
mkdirSync(join(output, "logs"));
mkdirSync(join(output, "tmp"));
mkdirSync(artifacts);
const source = captureSource(root);
freezeSource(root, output, source);
const work = prepareExecution(root, output, source);
const executionSource = captureSource(work);
const toolchain = readFileSync(
  join(work, "rust/rust-toolchain.toml"),
  "utf8",
).match(/channel\s*=\s*"([^"]+)"/)?.[1];
if (!toolchain) throw new Error("Rust toolchain pin missing");
for (const entry of gates) {
  entry.cwd = join(work, relative(root, entry.cwd));
  entry.requireTests = [
    "rust-tests",
    "native-tests",
    "bun-full",
    "differential",
  ].includes(entry.id);
  entry.noSkips = entry.id === "native-tests";
  entry.env = {
    RUSTUP_TOOLCHAIN: toolchain,
    CARGO_TARGET_DIR: undefined,
    NODE_OPTIONS: "",
    NODE_PATH: "",
    NAPI_RS_NATIVE_LIBRARY_PATH: "",
    NO_COLOR: "1",
    FORCE_COLOR: "0",
    TMPDIR: join(output, "tmp"),
    npm_config_cache: join(output, "npm-cache"),
    BUN_INSTALL_CACHE_DIR: join(output, "bun-cache"),
    ...entry.env,
  };
  if (
    ![
      "install",
      "local-actions",
      "rust-format",
      "rust-tests",
      "rust-clippy",
      "rust-packages",
    ].includes(entry.id)
  )
    entry.requires = [...new Set(["install", ...(entry.requires ?? [])])];
}
const versions = {};
for (const [name, command, commandArgs] of [
  ["node", process.execPath, ["--version"]],
  ["bun", "bun", ["--version"]],
  ["npm", "npm", ["--version"]],
  ["cargo", "cargo", ["--version"]],
  ["rustc", "rustc", ["--version"]],
]) {
  const result = spawnSync(command, commandArgs, {
    encoding: "utf8",
    timeout: 5000,
    env: { ...process.env, RUSTUP_TOOLCHAIN: toolchain },
  });
  versions[name] = {
    command,
    args: commandArgs,
    version: result.stdout?.trim(),
    exitCode: result.status,
    error: result.error?.message ?? null,
  };
}
const bunPath = spawnSync("bun", ["-e", "console.log(process.execPath)"], {
  encoding: "utf8",
  timeout: 5000,
});
const runtimeEnvironment = {
  ...gates.find(({ id }) => id === "artifact-runtime").env,
  RUSTUP_TOOLCHAIN: toolchain,
  AGENTKIT_NODE_BINARY: process.execPath,
  ...(bunPath.status === 0
    ? { AGENTKIT_BUN_BINARY: bunPath.stdout.trim() }
    : {}),
};
gates.find(({ id }) => id === "artifact-runtime").env = runtimeEnvironment;
for (const [name, matches] of runtimeSpecs) {
  if (!runtimeInputs[name]) continue;
  const result = spawnSync(runtimeInputs[name], ["--version"], {
    encoding: "utf8",
    timeout: 5000,
  });
  versions[name] = {
    command: runtimeInputs[name],
    version: result.stdout?.trim(),
    exitCode: result.status,
    error: result.error?.message ?? null,
  };
  const entry = gates.find(({ id }) => id === `runtime-${name}`);
  if (result.status !== 0 || !matches.test(versions[name].version ?? ""))
    entry.blockedReason = `Required runtime ${name} unavailable or version mismatch`;
  entry.env = {
    ...runtimeEnvironment,
    [entry.runtimeFamily === "node"
      ? "AGENTKIT_NODE_BINARY"
      : "AGENTKIT_BUN_BINARY"]: runtimeInputs[name],
  };
}
const rows = [];
let sourceStable = true;
for (const entry of gates) {
  const omitted = selected && !selected.has(entry.id);
  const unmet = entry.requires?.filter(
    (id) => rows.find((row) => row.id === id)?.status !== "PASS",
  );
  if (omitted)
    rows.push({
      id: entry.id,
      status: "NOT RUN",
      reason: "Excluded by --only",
    });
  else if (entry.blockedReason)
    rows.push({ id: entry.id, status: "BLOCKED", reason: entry.blockedReason });
  else if (!sourceStable)
    rows.push({
      id: entry.id,
      status: "BLOCKED",
      reason: "Source changed during qualification",
    });
  else if (unmet?.length)
    rows.push({
      id: entry.id,
      status: "BLOCKED",
      reason: `Unmet gates: ${unmet.join(", ")}`,
    });
  else {
    console.log(`RUN ${entry.id}`);
    rows.push(await runGate(entry, join(output, "logs", `${entry.id}.log`)));
    const nested = join(output, "cold-packages", "qualification.json");
    if (entry.id === "cold-packages" && existsSync(nested)) {
      const status = JSON.parse(readFileSync(nested, "utf8")).status;
      if (["BLOCKED", "INCONCLUSIVE"].includes(status))
        rows.at(-1).status = status;
    }
    console.log(`${rows.at(-1).status} ${entry.id}`);
    sourceStable = sameSource(executionSource, captureSource(work));
  }
  writeFileSync(
    join(output, "progress.json"),
    `${JSON.stringify(rows, null, 2)}\n`,
  );
}
rows.push({
  id: "source-stability",
  status: sourceStable ? "PASS" : "INCONCLUSIVE",
});
const runtimeCells = runtimeSpecs.map(([name, matches, family]) => {
  const version = (versions[name] ?? versions[family]).version;
  const runtimeGate = runtimeInputs[name]
    ? `runtime-${name}`
    : "artifact-runtime";
  return {
    name,
    version: matches.test(version ?? "") ? version : null,
    status: matches.test(version ?? "")
      ? (rows.find((row) => row.id === runtimeGate)?.status ?? "NOT RUN")
      : "BLOCKED",
    reason: matches.test(version ?? "")
      ? `Installed artifact lifecycle; see ${runtimeGate} log`
      : "Required runtime unavailable in this invocation",
  };
});
const platforms = NATIVE_PLATFORM_TARGETS.map(({ triple }) => ({
  triple,
  status: triple === target.triple ? overallStatus(runtimeCells) : "NOT RUN",
  reason:
    triple === target.triple
      ? "Host execution only; runtime cells reported separately"
      : "Outside current local host qualification scope",
}));
const files = [];
function collect(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (
        ![
          "source",
          "work",
          "node_modules",
          "target",
          "bun-cache",
          "npm-cache",
          "tmp",
        ].includes(entry.name) &&
        !entry.name.startsWith("stage-")
      )
        collect(path);
    } else if (entry.isFile() && !path.endsWith("progress.json")) {
      files.push({
        path: relative(output, path),
        sha256: digest(readFileSync(path)),
      });
    }
  }
}
collect(output);
const report = {
  format: "agentkit-local-qualification-v1",
  source,
  platform: `${process.platform}-${process.arch}`,
  scope: { kind: "local-host", target: target.triple },
  versions,
  gates: rows,
  runtimeCells,
  platforms,
  status: overallStatus([
    ...rows,
    ...platforms.filter(({ triple }) => triple === target.triple),
  ]),
  files,
  finishedAt: new Date().toISOString(),
  published: false,
};
writeFileSync(
  join(output, "qualification.json"),
  `${JSON.stringify(report, null, 2)}\n`,
  { flag: "wx" },
);
for (const file of files) chmodSync(join(output, file.path), 0o444);
chmodSync(join(output, "qualification.json"), 0o444);
console.log(`${report.status}: ${join(output, "qualification.json")}`);
process.exitCode = report.status === "PASS" ? 0 : 1;
