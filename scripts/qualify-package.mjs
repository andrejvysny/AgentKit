#!/usr/bin/env node
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const options = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, arg, i, args) => {
    if (arg.startsWith("--")) pairs.push([arg.slice(2), args[i + 1]]);
    return pairs;
  }, []),
);
if (
  !options.tarball ||
  !/^[a-f0-9]{64}$/.test(options["source-digest"] ?? "")
) {
  throw new Error(
    "Usage: node scripts/qualify-package.mjs --tarball PATH --source-digest SHA256 [--output DIR] [--sqlite EXACT_VERSION] [--electron EXACT_VERSION] [--responses true] [--migration-from FOUNDATION_TARBALL]",
  );
}
const sqliteVersion = options.sqlite ?? "13.0.3";
if (!/^\d+\.\d+\.\d+$/.test(sqliteVersion))
  throw new Error("SQLite driver version must be exact");
const tarball = resolve(options.tarball);
const output = options.output
  ? resolve(options.output)
  : mkdtempSync(join(tmpdir(), "agentkit-qualification-"));
mkdirSync(output, { recursive: true });
const digest = createHash("sha256").update(readFileSync(tarball)).digest("hex");
const fixtureFiles = [
  ["consumer.mjs", join(root, "scripts", "qualification", "consumer.mjs")],
  ["types.ts", join(root, "scripts", "qualification", "types.ts")],
  [
    "electron-native.cjs",
    join(root, "scripts", "qualification", "electron-native.cjs"),
  ],
  ["node-host.mjs", join(root, "examples", "desktop-host", "node-host.mjs")],
];
if (options.responses)
  fixtureFiles.push(
    [
      "proposals-smoke.mjs",
      join(root, "scripts", "qualification", "proposals-smoke.mjs"),
    ],
    [
      "migration-seed.mjs",
      join(root, "scripts", "qualification", "migration-seed.mjs"),
    ],
    [
      "responses-smoke.mjs",
      join(root, "scripts", "qualification", "responses-smoke.mjs"),
    ],
    [
      "responses-openpcb-smoke.mjs",
      join(root, "scripts", "qualification", "responses-openpcb-smoke.mjs"),
    ],
    [
      "openpcb-catalog.json",
      join(root, "packages", "testing", "fixtures", "openpcb", "catalog.json"),
    ],
  );
const capturedFixtures = fixtureFiles.map(([name, path]) => ({
  name,
  bytes: readFileSync(path),
}));
const evidence = {
  artifact: tarball,
  sha256: digest,
  sourceDigest: options["source-digest"],
  node: process.version,
  platform: process.platform,
  arch: process.arch,
  nodeAbi: process.versions.modules,
  sqliteVersion,
  electronRuntimeQualified: false,
  checks: [],
  fixtures: capturedFixtures.map(({ name, bytes }) => ({
    name,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  })),
};

function record() {
  writeFileSync(
    join(output, "qualification.json"),
    `${JSON.stringify(evidence, null, 2)}\n`,
  );
}

function run(command, args, cwd, label, environment = {}) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    timeout: 180000,
    env: {
      ...process.env,
      npm_config_cache: join(output, "npm-cache"),
      BUN_INSTALL_CACHE_DIR: join(output, "bun-cache"),
      ELECTRON_CACHE: join(output, "electron-cache"),
      electron_config_cache: join(output, "electron-cache"),
      ...environment,
    },
  });
  const log = `${result.stdout ?? ""}${result.stderr ?? ""}${result.error?.message ?? ""}`;
  writeFileSync(join(output, `${label}.log`), log);
  evidence.checks.push({
    name: label,
    command: [command, ...args],
    status: result.status,
    passed: result.status === 0 && !result.error,
  });
  record();
  if (result.status !== 0 || result.error)
    throw new Error(
      `${label} failed; see ${join(output, `${label}.log`)}\n${log}`,
    );
  return log.trim();
}

function writeConsumerManifest(directory, manager, artifact = tarball) {
  writeFileSync(
    join(directory, "package.json"),
    `${JSON.stringify(
      {
        name: `agentkit-${manager}-qualification`,
        private: true,
        type: "module",
        trustedDependencies: ["better-sqlite3", "esbuild"],
        dependencies: {
          agentkit: `file:${artifact}`,
          react: "19.2.0",
          "better-sqlite3": sqliteVersion,
        },
        devDependencies: {
          esbuild: "0.25.12",
          typescript: "5.9.3",
          "@types/node": "^22",
          "@types/react": "^19",
          "node-gyp": "10.3.1",
        },
      },
      null,
      2,
    )}\n`,
  );
}

function prepareConsumer(manager, artifact = tarball) {
  const directory = join(output, `${manager}-consumer`);
  mkdirSync(directory);
  writeConsumerManifest(directory, manager, artifact);
  for (const { name, bytes } of capturedFixtures)
    writeFileSync(join(directory, name), bytes);
  writeFileSync(
    join(directory, "tsconfig.json"),
    JSON.stringify({
      compilerOptions: {
        strict: true,
        noEmit: true,
        target: "ES2022",
        module: "NodeNext",
        moduleResolution: "NodeNext",
        types: ["node", "react"],
        skipLibCheck: false,
      },
      include: ["types.ts"],
    }),
  );
  return directory;
}

function qualify(manager) {
  const directory = prepareConsumer(manager);
  const args =
    manager === "npm" ? ["install", "--no-audit", "--no-fund"] : ["install"];
  run(manager, args, directory, `${manager}-install`);
  run(
    "node",
    ["consumer.mjs"],
    directory,
    `${manager}-node-import-bundle-native-host`,
  );
  run(
    "npx",
    ["--no-install", "tsc", "-p", "tsconfig.json"],
    directory,
    `${manager}-types`,
  );
  qualifyNodeNative(directory, manager);
  if (options.responses) {
    run(
      "node",
      ["responses-smoke.mjs"],
      directory,
      `${manager}-responses-reopen`,
    );
    run(
      "node",
      ["responses-openpcb-smoke.mjs"],
      directory,
      `${manager}-responses-openpcb-reopen`,
    );
  }
  if (manager === "bun")
    run(
      "bun",
      [
        "-e",
        `import { SqliteAssistantStore } from 'agentkit/adapters-sqlite'; const s = new SqliteAssistantStore(':memory:'); s.close(); console.log('Bun SQLite loaded');`,
      ],
      directory,
      "bun-sqlite",
    );
}

function qualifyNodeNative(directory, manager) {
  const runtime = { AGENTKIT_EXPECT_RUNTIME: "node" };
  run(
    "node",
    ["electron-native.cjs", "node-native.sqlite", "seed"],
    directory,
    `${manager}-node-native-seed`,
    runtime,
  );
  const reopened = parseRuntime(
    run(
      "node",
      ["electron-native.cjs", "node-native.sqlite", "reopen"],
      directory,
      `${manager}-node-native-process-reopen`,
      runtime,
    ),
  );
  if (
    reopened.nativePackage.version !== sqliteVersion ||
    !reopened.processReopened
  )
    throw new Error("Unexpected Node native driver or reopen result");
  evidence[`${manager}NativeRuntime`] = reopened;
  record();
}

function qualifyMigration() {
  const previous = resolve(options["migration-from"]);
  const previousDigest = createHash("sha256")
    .update(readFileSync(previous))
    .digest("hex");
  const directory = prepareConsumer("migration", previous);
  run(
    "npm",
    ["install", "--no-audit", "--no-fund"],
    directory,
    "migration-a-install",
  );
  const database = join(output, "migration-8-to-9.sqlite");
  run("node", ["migration-seed.mjs", database], directory, "migration-a-seed");
  run(
    "node",
    ["responses-smoke.mjs", database, `${database}.seed.json`],
    join(output, "npm-consumer"),
    "migration-b-responses-reopen",
  );
  if (
    createHash("sha256").update(readFileSync(previous)).digest("hex") !==
    previousDigest
  )
    throw new Error("Foundation artifact changed");
  evidence.migration = {
    artifact: previous,
    sha256: previousDigest,
    from: 8,
    to: 9,
    populated: true,
    reopened: true,
  };
  record();
}

function prepareElectronConsumer(version) {
  if (!/^\d+\.\d+\.\d+$/.test(version))
    throw new Error("Electron version must be exact");
  const directory = prepareConsumer("electron");
  run(
    "npm",
    [
      "install",
      "--save-dev",
      "--save-exact",
      `electron@${version}`,
      "--no-audit",
      "--no-fund",
    ],
    directory,
    "electron-install",
  );
  run("node", ["consumer.mjs"], directory, "electron-consumer-node-bundle");
  run(
    "node",
    [
      "-e",
      "const fs = require('node:fs'); const path = require('electron'); if (!fs.existsSync(path)) throw new Error('Electron executable missing'); console.log(fs.realpathSync(path));",
    ],
    directory,
    "electron-binary-install",
  );
  return directory;
}

function qualifyElectron(version) {
  const directory = prepareElectronConsumer(version);
  const environment = { ELECTRON_RUN_AS_NODE: "1" };
  const command = join(directory, "node_modules", ".bin", "electron");
  const before = parseRuntime(
    run(
      command,
      ["electron-native.cjs", "electron-native.sqlite", "seed"],
      directory,
      "electron-native-before-rebuild",
      environment,
    ),
  );
  run(
    "npm",
    ["rebuild", "better-sqlite3", "--verbose"],
    directory,
    "electron-npm-rebuild",
  );
  const after = parseRuntime(
    run(
      command,
      ["electron-native.cjs", "electron-native.sqlite", "reopen"],
      directory,
      "electron-native-after-rebuild",
      environment,
    ),
  );
  if (before.electron !== version || after.electron !== version)
    throw new Error("Unexpected Electron version");
  if (
    before.nativePackage.version !== sqliteVersion ||
    after.nativePackage.version !== sqliteVersion ||
    !after.processReopened
  )
    throw new Error("Unexpected Electron native driver or reopen result");
  evidence.electron = {
    before,
    after,
    mode: "ELECTRON_RUN_AS_NODE=1",
    guiQualified: false,
    rebuild:
      "npm rebuild lifecycle check only; inspect logs and loaded binary metadata; no source recompilation claimed",
  };
  evidence.electronRuntimeQualified = true;
  record();
}

function parseRuntime(log) {
  const line = log.split("\n").find((line) => line.startsWith("{"));
  if (!line) throw new Error("Electron runtime did not report metadata");
  return JSON.parse(line);
}

record();
evidence.npm = run("npm", ["--version"], root, "npm-version");
evidence.bun = run("bun", ["--version"], root, "bun-version");
const packed = JSON.parse(
  run(
    "tar",
    ["-xzOf", tarball, "package/package.json"],
    root,
    "artifact-manifest",
  ),
);
evidence.package = { name: packed.name, version: packed.version };
if (JSON.stringify(packed).includes("workspace:"))
  throw new Error("Packed manifest contains workspace dependency");
if (packed.name !== "agentkit")
  throw new Error("Expected packed agentkit umbrella");
qualify("npm");
qualify("bun");
if (options["migration-from"]) {
  if (!options.responses)
    throw new Error("Migration qualification requires --responses true");
  qualifyMigration();
}
if (options.electron) qualifyElectron(options.electron);
if (createHash("sha256").update(readFileSync(tarball)).digest("hex") !== digest)
  throw new Error("Artifact changed during qualification");
evidence.passed = true;
record();
console.log(`Qualified exact tarball ${digest}; evidence: ${output}`);
console.log(
  evidence.electronRuntimeQualified
    ? "Electron native runtime qualified in RunAsNode mode; no GUI or ABI recompilation claim. No publication performed."
    : "Electron runtime/ABI not qualified. No publication performed.",
);
