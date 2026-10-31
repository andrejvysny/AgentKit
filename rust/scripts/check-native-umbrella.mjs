#!/usr/bin/env node
/** Installed umbrella native and default-backend acceptance, outside the checkout. */
import { spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { stageNativePlatform } from "./stage-native-platform.mjs";
import {
  assertNoRust,
  findRuntime,
  findNpmCli,
  pathEntries,
  toolchainFreePath,
  withPath,
} from "./native-commands.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const temporary = mkdtempSync(join(tmpdir(), "agentkit-umbrella-native-"));

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: "inherit",
    timeout: 180_000,
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(`${command} ${args.join(" ")} failed: ${result.status}`);
}
try {
  const stage = join(temporary, "stage");
  const umbrella = join(stage, "agentkit");
  mkdirSync(umbrella, { recursive: true });
  for (const entry of ["package.json", "README.md", "dist"])
    cpSync(join(root, "packages/agentkit", entry), join(umbrella, entry), {
      recursive: true,
    });
  const platform = stageNativePlatform(root, stage);
  const tarballs = join(temporary, "tarballs");
  mkdirSync(tarballs);
  const paths = pathEntries(process.env.PATH ?? process.env.Path);
  const bun = findRuntime("bun", paths);
  if (!bun) throw new Error("missing bun");
  const node = process.execPath;
  const npm = findNpmCli(node, paths);
  const cleanPath = toolchainFreePath([dirname(bun), dirname(node), ...paths]);
  const env = {
    ...withPath(process.env, cleanPath),
    npm_config_cache: join(temporary, "npm-cache"),
  };
  delete env.AGENTKIT_BACKEND;
  delete env.NODE_PATH;
  delete env.NAPI_RS_NATIVE_LIBRARY_PATH;
  delete env.NAPI_RS_FORCE_WASI;
  delete env.NAPI_RS_WASI_FLAVOR;
  assertNoRust(env, cleanPath);
  run(node, [npm, "pack", "--pack-destination", tarballs], {
    cwd: umbrella,
    env,
  });
  run(node, [npm, "pack", "--pack-destination", tarballs], {
    cwd: platform.destination,
    env,
  });
  const manifest = JSON.parse(
    readFileSync(join(umbrella, "package.json"), "utf8"),
  );
  const umbrellaTarball = join(tarballs, `agentkit-${manifest.version}.tgz`);
  const platformTarball = join(
    tarballs,
    `agentkit-native-${platform.target}-${manifest.optionalDependencies[`@agentkit/native-${platform.target}`]}.tgz`,
  );
  const consumer = join(temporary, "consumer");
  mkdirSync(consumer);
  writeFileSync(
    join(consumer, "package.json"),
    JSON.stringify({
      name: "agentkit-umbrella-consumer",
      private: true,
      type: "module",
      optionalDependencies: {
        [`@agentkit/native-${platform.target}`]: `file:${platformTarball}`,
      },
    }),
  );
  cpSync(
    join(root, "rust/scripts/native-umbrella-consumer.mjs"),
    join(consumer, "consumer.mjs"),
  );
  run(node, [npm, "install", "--no-audit", "--no-fund", umbrellaTarball], {
    cwd: consumer,
    env,
  });
  const addon = join(
    consumer,
    "node_modules/@agentkit",
    `native-${platform.target}`,
    platform.binaryName,
  );
  if (!existsSync(addon))
    throw new Error("matching optional platform addon not installed");
  const localAddons = join(consumer, "node_modules/agentkit/dist/generated");
  const archive = spawnSync("tar", ["-tzf", umbrellaTarball], {
    encoding: "utf8",
  });
  if (
    archive.status !== 0 ||
    archive.stdout.split("\n").some((entry) => entry.endsWith(".node"))
  )
    throw new Error(
      `umbrella tarball contains an addon or is unreadable: ${localAddons}`,
    );
  for (const runtime of [node, bun])
    run(runtime, ["consumer.mjs", "native"], {
      cwd: consumer,
      env: { ...env, AGENTKIT_BACKEND: "native" },
    });
  rmSync(
    join(consumer, "node_modules/@agentkit", `native-${platform.target}`),
    { recursive: true },
  );
  for (const runtime of [node, bun]) {
    run(runtime, ["consumer.mjs", "ts"], { cwd: consumer, env });
    run(runtime, ["consumer.mjs", "missing"], {
      cwd: consumer,
      env: { ...env, AGENTKIT_BACKEND: "native" },
    });
  }
  process.stdout.write(
    `umbrella acceptance passed on ${platform.target}; no Rust toolchain reachable\n`,
  );
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
