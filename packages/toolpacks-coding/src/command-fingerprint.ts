import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { access, open, realpath, stat } from "node:fs/promises";
import {
  delimiter,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
} from "node:path";
import { sanitizedEnv } from "./exec-tools.js";
import { executableName, shellWords } from "./command-words.js";
import {
  directScriptFiles,
  leadingExecutable,
  scriptExecutable,
} from "./command-script-inputs.js";

const MAX_FILE_BYTES = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES = 32 * 1024 * 1024;
const MAX_FILES = 128;
const MAX_ANCESTORS = 16;
const MAX_ENV_BYTES = 128 * 1024;
const ENV_NAMES = new Set([
  "PATH",
  "HOME",
  "SHELL",
  "BASH_ENV",
  "ENV",
  "LANG",
  "LC_ALL",
  "LC_CTYPE",
  "NODE_ENV",
  "NODE_OPTIONS",
  "NODE_PATH",
  "BUN_OPTIONS",
  "BUN_INSTALL",
  "PYTHONPATH",
  "PYTHONHOME",
  "VIRTUAL_ENV",
  "PYTEST_ADDOPTS",
  "UV_PROJECT_ENVIRONMENT",
  "CARGO_HOME",
  "RUSTUP_HOME",
  "RUSTUP_TOOLCHAIN",
  "RUSTFLAGS",
  "RUSTDOCFLAGS",
  "GOPATH",
  "GOROOT",
  "GOFLAGS",
  "GOWORK",
  "GOTOOLCHAIN",
  "MAKEFLAGS",
  "LD_LIBRARY_PATH",
  "LD_PRELOAD",
  "DYLD_LIBRARY_PATH",
  "DYLD_INSERT_LIBRARIES",
  "npm_config_prefix",
  "npm_config_userconfig",
  "npm_config_registry",
]);

function executionEnvironment(source: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const env = Object.fromEntries(
    Object.entries(sanitizedEnv(source, "/standing-approval"))
      .filter(([name]) => ENV_NAMES.has(name))
      .sort(([a], [b]) => a.localeCompare(b)),
  );
  if (
    Buffer.byteLength(JSON.stringify(env)) > MAX_ENV_BYTES ||
    (env.PATH ?? "").split(delimiter).length > 128
  )
    throw new Error("standing approval environment exceeds bounds");
  return env;
}

const METADATA = [
  "package.json",
  "bun.lock",
  "bun.lockb",
  "package-lock.json",
  "npm-shrinkwrap.json",
  "yarn.lock",
  "pnpm-lock.yaml",
  "pnpm-workspace.yaml",
  ".npmrc",
  ".yarnrc.yml",
  "bunfig.toml",
  ".bunfig.toml",
  ".node-version",
  ".nvmrc",
  ".tool-versions",
  "Cargo.toml",
  "Cargo.lock",
  ".cargo/config",
  ".cargo/config.toml",
  "rust-toolchain.toml",
  "rust-toolchain",
  "go.mod",
  "go.sum",
  "go.work",
  "go.work.sum",
  "Makefile",
  "makefile",
  "GNUmakefile",
  "pyproject.toml",
  "uv.lock",
  "pytest.ini",
  "tox.ini",
  "setup.cfg",
  "requirements.txt",
  ".python-version",
  "tsconfig.json",
  "jsconfig.json",
  ".eslintrc",
  ".eslintrc.json",
  ".prettierrc",
  ".prettierrc.json",
  ".prettierignore",
  "biome.json",
];
const CONFIG_STEMS = [
  "vitest.config",
  "vite.config",
  "jest.config",
  "eslint.config",
  "prettier.config",
];
const CONFIG_EXTENSIONS = ["ts", "js", "mts", "mjs", "cts", "cjs", "json"];

function ancestors(root: string, cwd: string): string[] | null {
  const paths: string[] = [];
  for (let dir = cwd; ; dir = dirname(dir)) {
    paths.push(dir);
    if (paths.length > MAX_ANCESTORS) return null;
    if (dir === root) return paths;
    if (dir === dirname(dir)) return null;
  }
}

async function executable(file: string, cwd: string, env: NodeJS.ProcessEnv) {
  const paths = file.includes("/")
    ? [resolve(cwd, file)]
    : (env.PATH ?? "/usr/bin:/bin")
        .split(delimiter)
        .map((p) => resolve(cwd, p, file));
  for (const path of paths) {
    try {
      await access(path, constants.X_OK);
      const canonical = await realpath(path);
      const info = await stat(canonical, { bigint: true });
      if (!info.isFile()) continue;
      return {
        path,
        canonical,
        identity: [
          info.dev,
          info.ino,
          info.mode,
          info.size,
          info.mtimeNs,
          info.ctimeNs,
        ].map(String),
      };
    } catch (error) {
      if (!isAbsent(error)) throw error;
    }
  }
  return null;
}

function isAbsent(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error.code === "ENOENT" ||
      error.code === "ENOTDIR" ||
      error.code === "EACCES")
  );
}

interface FingerprintFiles {
  bytes: number;
  count: number;
  entries: unknown[];
  packages: Map<string, Buffer>;
}

async function boundedContents(
  path: string,
  expectedSize: number,
): Promise<Buffer> {
  const handle = await open(path, "r");
  try {
    const buffer = Buffer.alloc(expectedSize + 1);
    let length = 0;
    while (length < buffer.length) {
      const { bytesRead } = await handle.read(
        buffer,
        length,
        buffer.length - length,
        length,
      );
      if (bytesRead === 0) break;
      length += bytesRead;
    }
    if (length > expectedSize)
      throw new Error("standing approval metadata changed while reading");
    return buffer.subarray(0, length);
  } finally {
    await handle.close();
  }
}

async function bindFile(
  path: string,
  state: FingerprintFiles,
  required = false,
): Promise<void> {
  try {
    const canonical = await realpath(path);
    const info = await stat(canonical);
    if (
      !info.isFile() ||
      info.size > MAX_FILE_BYTES ||
      state.bytes + info.size > MAX_TOTAL_BYTES
    )
      throw new Error("standing approval metadata exceeds bounds");
    if (++state.count > MAX_FILES)
      throw new Error("too many standing approval inputs");
    const contents = await boundedContents(canonical, info.size);
    if (
      contents.length > MAX_FILE_BYTES ||
      state.bytes + contents.length > MAX_TOTAL_BYTES
    )
      throw new Error("standing approval metadata exceeds bounds");
    state.bytes += contents.length;
    if (path.endsWith("/package.json")) state.packages.set(path, contents);
    state.entries.push([
      path,
      canonical,
      createHash("sha256").update(contents).digest("hex"),
    ]);
  } catch (error) {
    if (required) throw error;
    if (!isAbsent(error)) throw error;
    // Missing metadata is covered too: its later appearance changes the key.
    if (
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === "EACCES"
    )
      throw error;
    state.entries.push([path, null]);
  }
}

async function bindDirectFile(
  root: string,
  path: string,
  state: FingerprintFiles,
): Promise<void> {
  const canonical = await realpath(path);
  const inside = relative(root, canonical);
  if (isAbsolute(inside) || inside === ".." || inside.startsWith("../"))
    throw new Error("direct command input outside workspace");
  await bindFile(path, state, true);
}

async function bindMetadata(
  dirs: string[],
  state: FingerprintFiles,
): Promise<void> {
  const configs = CONFIG_STEMS.flatMap((stem) =>
    CONFIG_EXTENSIONS.map((ext) => `${stem}.${ext}`),
  );
  for (const dir of dirs) {
    for (const name of [...METADATA, ...configs])
      await bindFile(join(dir, name), state);
  }
}

async function bindScriptEntrypoints(
  root: string,
  dirs: string[],
  tokens: string[],
  env: NodeJS.ProcessEnv,
  executables: object[],
  state: FingerprintFiles,
) {
  const script =
    tokens[1] === "run" ? tokens[2] : tokens[1] === "test" ? "test" : undefined;
  if (script === undefined) return;
  for (const dir of dirs) {
    const contents = state.packages.get(join(dir, "package.json"));
    if (contents === undefined) continue;
    const pkg = JSON.parse(contents.toString("utf8")) as {
      scripts?: Record<string, unknown>;
    };
    const scripts = pkg.scripts;
    for (const name of [script, `pre${script}`, `post${script}`]) {
      const command = scripts?.[name];
      if (typeof command !== "string") continue;
      for (const word of directScriptFiles(command))
        await bindDirectFile(root, resolve(dir, word), state);
      await bindExplicitConfigs(root, dir, shellWords(command), {}, state);
      const bare = scriptExecutable(command);
      if (bare !== undefined)
        executables.push({
          script: bare,
          ...(await scriptRunner(dir, bare, env)),
        });
    }
  }
}

/** Mirrors npm run-script PATH: every ancestor node_modules/.bin, then PATH. */
async function scriptRunner(dir: string, name: string, env: NodeJS.ProcessEnv) {
  for (let a = dir; ; a = dirname(a)) {
    const local = await executable(
      join(a, "node_modules", ".bin", name),
      dir,
      env,
    );
    if (local !== null) return local;
    if (a === dirname(a)) break;
  }
  const found = await executable(name, dir, env);
  if (found === null)
    throw new Error("package script executable is unresolved");
  return found;
}

async function bindExplicitConfigs(
  root: string,
  cwd: string,
  tokens: string[],
  env: NodeJS.ProcessEnv,
  state: FingerprintFiles,
): Promise<void> {
  const flags = new Set([
    "--config",
    "--project",
    "--manifest-path",
    "--modfile",
    "--preload",
    "--require",
    "--import",
    "--loader",
  ]);
  const typescript = leadingExecutable(tokens) === "tsc";
  if (typescript) flags.add("-p");
  if (tokens.includes("pytest")) flags.add("-c");
  if (tokens.some((token) => /^(node(js)?|bun)$/.test(executableName(token))))
    flags.add("-r");
  for (const [index, token] of tokens.entries()) {
    const equal = token.indexOf("=");
    const short = /^-[pcr].+/.test(token) && flags.has(token.slice(0, 2));
    const flag = short
      ? token.slice(0, 2)
      : equal === -1
        ? token
        : token.slice(0, equal);
    const attached = short
      ? token.slice(2)
      : equal === -1
        ? undefined
        : token.slice(equal + 1);
    if (!flags.has(flag!)) continue;
    const file = attached ?? tokens[index + 1];
    if (file === undefined || file === "" || file.startsWith("-"))
      throw new Error("missing runner configuration");
    const path = resolve(cwd, file);
    const info = await stat(path);
    const projectDirectory =
      info.isDirectory() &&
      typescript &&
      (flag === "--project" || flag === "-p");
    if (info.isDirectory() && !projectDirectory)
      throw new Error("direct command input must be a file");
    await bindDirectFile(
      root,
      projectDirectory ? join(path, "tsconfig.json") : path,
      state,
    );
  }
  for (const name of ["BASH_ENV", "ENV", "npm_config_userconfig"]) {
    const file = env[name];
    if (file !== undefined)
      await bindDirectFile(root, resolve(cwd, file), state);
  }
}

/** Bounded configuration identity, never a claim about arbitrary transitive code. */
export async function commandFingerprint(
  root: string,
  cwd: string,
  command: string,
  environment: NodeJS.ProcessEnv,
): Promise<string | null> {
  const dirs = ancestors(root, cwd);
  if (dirs === null) return null;
  const env = executionEnvironment(environment);
  const tokens = command.trim().split(/ +/);
  const main = await executable(tokens[0]!, cwd, env);
  if (main === null) return null;
  const executables: object[] = [main];
  if (tokens[0] === "npx" || tokens[0] === "bunx") {
    const local = dirs.map((dir) =>
      join(dir, "node_modules", ".bin", tokens[1]!),
    );
    let runner = null;
    for (const path of local) {
      runner = await executable(path, cwd, env);
      if (runner !== null) break;
    }
    runner ??= await executable(tokens[1]!, cwd, env);
    if (runner === null) return null;
    executables.push(runner);
  }
  const state: FingerprintFiles = {
    bytes: 0,
    count: 0,
    entries: [],
    packages: new Map(),
  };
  await bindMetadata(dirs, state);
  await bindScriptEntrypoints(root, dirs, tokens, env, executables, state);
  await bindExplicitConfigs(root, cwd, tokens, env, state);
  return createHash("sha256")
    .update(
      JSON.stringify({
        v: 1,
        executables,
        files: state.entries,
        environment: Object.entries(env)
          .filter(([, value]) => value !== undefined)
          .sort(([a], [b]) => a.localeCompare(b)),
      }),
    )
    .digest("hex");
}
