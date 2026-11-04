import { spawnSync } from "node:child_process";
import {
  constants,
  accessSync,
  existsSync,
  realpathSync,
  statSync,
} from "node:fs";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";

const RUST_TOOLS = ["cargo", "rustc", "rustup"];

export function pathEntries(path, platform = process.platform) {
  return (path ?? "")
    .split(platform === "win32" ? ";" : delimiter)
    .map((entry) => entry.replace(/^"(.*)"$/, "$1"))
    .filter(Boolean);
}

export function executableNames(name, platform = process.platform) {
  return platform === "win32" ? [`${name}.exe`, `${name}.com`] : [name];
}

export function findExecutable(name, paths, platform = process.platform) {
  for (const directory of paths) {
    for (const file of executableNames(name, platform)) {
      const candidate = resolve(directory, file);
      try {
        if (!statSync(candidate).isFile()) continue;
        if (platform !== "win32") accessSync(candidate, constants.X_OK);
        return candidate;
      } catch {
        /* Continue searching PATH. */
      }
    }
  }
  return undefined;
}

export function findRuntime(name, paths, platform = process.platform) {
  for (const directory of paths) {
    const candidate = findExecutable(name, [directory], platform);
    if (!candidate) continue;
    const result = spawnSync(candidate, ["--version"], { stdio: "ignore" });
    if (!result.error && result.status === 0) return candidate;
  }
  return undefined;
}

export function findNpmCli(nodeBinary, paths, platform = process.platform) {
  const nodeDir = dirname(realpathSync(nodeBinary));
  const dirs = [...new Set([...paths, nodeDir])];
  const candidates = dirs.flatMap((dir) => [
    join(dir, "node_modules/npm/bin/npm-cli.js"),
    join(dir, "../lib/node_modules/npm/bin/npm-cli.js"),
    ...(platform === "win32" ? [] : [join(dir, "npm")]),
  ]);
  for (const candidate of candidates) {
    try {
      if (!statSync(candidate).isFile()) continue;
      const script = realpathSync(candidate);
      const result = spawnSync(nodeBinary, [script, "--version"], {
        encoding: "utf8",
        stdio: "ignore",
      });
      if (!result.error && result.status === 0) return script;
    } catch {
      /* Continue searching installed npm distributions. */
    }
  }
  throw new Error("no usable npm-cli.js found beside Node or on PATH");
}

export function rustToolNames(platform = process.platform) {
  return RUST_TOOLS.flatMap((tool) =>
    platform === "win32"
      ? [
          tool,
          ...[".exe", ".com", ".cmd", ".bat"].map(
            (extension) => tool + extension,
          ),
        ]
      : [tool],
  );
}

export function toolchainFreePath(paths, platform = process.platform) {
  return [
    ...new Set(
      paths.filter(
        (entry) =>
          entry &&
          isAbsolute(entry) &&
          !/cargo|rustup|rustc|toolchain/i.test(entry) &&
          !rustToolNames(platform).some((tool) =>
            existsSync(join(entry, tool)),
          ),
      ),
    ),
  ];
}

export function withPath(env, paths, platform = process.platform) {
  const clean = Object.fromEntries(
    Object.entries(env).filter(([key]) => key.toUpperCase() !== "PATH"),
  );
  clean.PATH = paths.join(platform === "win32" ? ";" : delimiter);
  return clean;
}

export function assertNoRust(env, paths, platform = process.platform) {
  for (const directory of paths) {
    for (const name of rustToolNames(platform)) {
      if (existsSync(join(directory, name)))
        throw new Error(
          `Rust tool ${name} present in consumer PATH: ${directory}`,
        );
    }
  }
  for (const tool of RUST_TOOLS) {
    const result = spawnSync(tool, ["--version"], { env, stdio: "ignore" });
    if (!result.error)
      throw new Error(`${tool} reachable in consumer environment`);
  }
}
