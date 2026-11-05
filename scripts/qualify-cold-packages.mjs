#!/usr/bin/env node
/** Qualify package declarations from an isolated, declaration-cold source copy. */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = realpathSync(
  resolve(dirname(fileURLToPath(import.meta.url)), ".."),
);
const destination = process.argv[2];
if (!destination)
  throw new Error(
    "Usage: node scripts/qualify-cold-packages.mjs NEW_OUTPUT_DIRECTORY",
  );
const output = canonicalDestination(destination);
const within = relative(root, output);
if (
  existsSync(output) ||
  !(within === ".." || within.startsWith(`..${sep}`) || isAbsolute(within))
)
  throw new Error(
    "Qualification output must be a new directory outside the checkout",
  );
mkdirSync(output, { recursive: true });
const source = join(output, "source");
const environment = {
  ...process.env,
  TMPDIR: join(output, "tmp"),
  BUN_INSTALL_CACHE_DIR: join(output, "bun-cache"),
  npm_config_cache: join(output, "npm-cache"),
};
mkdirSync(environment.TMPDIR);
const commands = [];
const evidence = {
  status: "INCONCLUSIVE",
  platform: process.platform,
  architecture: process.arch,
  node: process.version,
  commands,
};
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const json = (path, value) =>
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);

function canonicalDestination(destination) {
  let current = resolve(destination);
  const suffix = [];
  while (!existsSync(current)) {
    suffix.unshift(basename(current));
    current = dirname(current);
  }
  return join(realpathSync(current), ...suffix);
}

function git(args) {
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error || result.status !== 0)
    throw result.error ?? new Error(result.stderr);
  return result.stdout;
}

function sourcePaths() {
  return [
    ...new Set(
      git(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .split("\0")
        .filter((path) => path && existsSync(join(root, path))),
    ),
  ].sort();
}

function snapshot() {
  const baseline = git(["rev-parse", "HEAD"]).trim();
  const dirty = git(["status", "--porcelain=v1", "--untracked-files=all"]);
  const patch = git(["diff", "--binary", "HEAD"]);
  const paths = sourcePaths();
  const files = paths.map((path) => {
    const input = join(root, path);
    if (!lstatSync(input).isFile())
      throw new Error(`Source must be a regular file: ${path}`);
    const sha256 = hash(readFileSync(input));
    const copied = join(source, path);
    mkdirSync(dirname(copied), { recursive: true });
    copyFileSync(input, copied);
    if (hash(readFileSync(copied)) !== sha256)
      throw new Error(`Copy raced: ${path}`);
    return { path, sha256 };
  });
  if (
    JSON.stringify(paths) !== JSON.stringify(sourcePaths()) ||
    files.some(
      (file) => hash(readFileSync(join(root, file.path))) !== file.sha256,
    )
  )
    throw new Error("Source changed during snapshot; rerun after writers stop");
  if (
    baseline !== git(["rev-parse", "HEAD"]).trim() ||
    patch !== git(["diff", "--binary", "HEAD"]) ||
    dirty !== git(["status", "--porcelain=v1", "--untracked-files=all"])
  )
    throw new Error("Git source changed during snapshot");
  writeFileSync(join(output, "dirty.patch"), patch);
  json(join(output, "source-manifest.json"), {
    algorithm: "sha256-sorted-source-manifest-v1",
    sha256: hash(JSON.stringify(files)),
    baseline,
    dirty,
    dirtyPatchSha256: hash(patch),
    files,
  });
  return baseline;
}

function run(label, command, args, options = {}) {
  const { expectedFailure = false, ...spawnOptions } = options;
  const startedAt = new Date().toISOString();
  const result = spawnSync(command, args, {
    cwd: source,
    env: environment,
    encoding: "utf8",
    timeout: 1_800_000,
    maxBuffer: 64 * 1024 * 1024,
    ...spawnOptions,
  });
  const log = `${commands.length + 1}-${label}.log`;
  writeFileSync(
    join(output, log),
    `${result.stdout ?? ""}${result.stderr ?? ""}${result.error ? `\n${result.error.message}\n` : ""}`,
  );
  const status =
    result.signal || result.error?.code === "ETIMEDOUT"
      ? "INCONCLUSIVE"
      : result.error
        ? "BLOCKED"
        : Number.isInteger(result.status) &&
            (result.status === 0) !== expectedFailure
          ? "PASS"
          : "FAIL";
  commands.push({
    label,
    command,
    args,
    cwd: options.cwd ?? source,
    startedAt,
    finishedAt: new Date().toISOString(),
    status,
    expectedFailure,
    exitCode: result.status,
    signal: result.signal,
    log,
    logSha256: hash(readFileSync(join(output, log))),
  });
  json(join(output, "qualification.json"), evidence);
  process.stdout.write(`${label}: ${status} (${log})\n`);
  return result;
}

function requirePass(label, command, args, options) {
  const result = run(label, command, args, options);
  if (result.error || result.status !== 0)
    throw new Error(`${label} failed; see ${output}`);
  return result;
}

function declarations(directory) {
  if (!existsSync(directory)) return [];
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory()
      ? declarations(path)
      : entry.name.endsWith(".d.ts")
        ? [
            {
              path: relative(source, path).split(sep).join("/"),
              sha256: hash(readFileSync(path)),
            },
          ]
        : [];
  });
}

function artifactHashes(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    if (
      [
        "source",
        "node_modules",
        "npm-cache",
        "bun-cache",
        "tmp",
        "target",
        ".git",
      ].includes(entry.name)
    )
      return [];
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return artifactHashes(path);
    return /\.(tgz|tar\.gz|node)$/.test(entry.name)
      ? [{ path: relative(output, path), sha256: hash(readFileSync(path)) }]
      : [];
  });
}

function checkExtractorFailure(bun) {
  const loader = join(output, "failed-extractor-loader.mjs");
  const fake =
    "export const ExtractorConfig={prepare: x=>x}; export const Extractor={invoke:()=>({succeeded:false})};";
  writeFileSync(
    loader,
    `export async function resolve(specifier, context, next) {\n  if (specifier === '@microsoft/api-extractor') return { url: ${JSON.stringify(`data:text/javascript,${encodeURIComponent(fake)}`)}, shortCircuit: true };\n  return next(specifier, context);\n}\n`,
  );
  const result = run(
    "extractor-failure-build",
    bun,
    ["run", "--cwd", "packages/native", "build"],
    {
      expectedFailure: true,
      env: {
        ...environment,
        NODE_OPTIONS: `--experimental-loader=${pathToFileURL(loader).href}`,
      },
    },
  );
  if (
    result.error ||
    result.signal ||
    !Number.isInteger(result.status) ||
    result.status === 0 ||
    !`${result.stdout}${result.stderr}`.includes(
      "api-extractor failed to roll up",
    )
  )
    throw new Error(
      "Injected unsuccessful extraction did not fail the package build as expected",
    );
}

try {
  evidence.baseline = snapshot();
  requirePass("freeze-source", "tar", [
    "-czf",
    join(output, "source.tar.gz"),
    "-C",
    source,
    ".",
  ]);
  evidence.sourceArchiveSha256 = hash(
    readFileSync(join(output, "source.tar.gz")),
  );
  evidence.authoritativeSource = "source.tar.gz";
  const bun = process.env.AGENTKIT_BUN_BINARY ?? "bun";
  for (const [label, command, args] of [
    ["node-version", process.execPath, ["--version"]],
    ["bun-version", bun, ["--version"]],
    ["cargo-version", "cargo", ["--version"]],
  ])
    requirePass(label, command, args);
  const packageDirectories = readdirSync(join(source, "packages"));
  const before = packageDirectories.flatMap((name) =>
    declarations(join(source, "packages", name, "dist")),
  );
  if (before.length !== 0)
    throw new Error(
      "Cold source copy unexpectedly contains package dist declarations",
    );
  evidence.coldDeclarations = before;
  requirePass("frozen-install", bun, ["install", "--frozen-lockfile"]);
  if (
    packageDirectories.some(
      (name) => declarations(join(source, "packages", name, "dist")).length,
    )
  )
    throw new Error(
      "Install warmed package declarations before the cold build",
    );
  requirePass("fresh-addon-build", bun, [
    "run",
    "--cwd",
    "packages/native",
    "build:native",
  ]);
  requirePass("cold-native-build", bun, [
    "run",
    "--cwd",
    "packages/native",
    "build",
  ]);
  const cold = declarations(join(source, "packages/native/dist"));
  if (cold.length !== 1)
    throw new Error(
      "Native build did not produce exactly one bundled declaration",
    );
  json(join(output, "cold-declarations.json"), cold);
  requirePass("cold-native-api", process.execPath, [
    "scripts/check-public-api.mjs",
    "--entry=native",
  ]);
  const nativeApiReport = join(
    source,
    "node_modules/.cache/agentkit-api/native.api.md",
  );
  copyFileSync(nativeApiReport, join(output, "cold-native.api.md"));
  for (const name of [
    "contracts",
    "client",
    "react",
    "core",
    "host",
    "mcp-client",
    "adapters-memory",
    "adapters-sqlite",
    "runner-local",
    "transport-http",
    "testing",
  ])
    requirePass(`build-${name}`, bun, ["run", `build:${name}`]);
  requirePass("warm-native-build", bun, ["run", "build:native"]);
  const warm = declarations(join(source, "packages/native/dist"));
  json(join(output, "warm-declarations.json"), warm);
  if (JSON.stringify(cold) !== JSON.stringify(warm))
    throw new Error("Cold and warm native declarations differ");
  evidence.declarationsEqual = true;
  requirePass("warm-native-api", process.execPath, [
    "scripts/check-public-api.mjs",
    "--entry=native",
  ]);
  copyFileSync(nativeApiReport, join(output, "warm-native.api.md"));
  const coldApi = hash(readFileSync(join(output, "cold-native.api.md")));
  const warmApi = hash(readFileSync(join(output, "warm-native.api.md")));
  if (coldApi !== warmApi)
    throw new Error("Cold/package and warm/root native API reports differ");
  evidence.nativeApiReportSha256 = coldApi;
  checkExtractorFailure(bun);
  // Restore the previously qualified bytes; do not hide the cold-build result behind a repair build.
  const coldDeclaration = join(source, cold[0].path);
  requirePass(
    "restore-native-declarations",
    process.execPath,
    [join(source, "scripts/prepack-bundle-types.mjs")],
    { cwd: join(source, "packages/native") },
  );
  if (hash(readFileSync(coldDeclaration)) !== cold[0].sha256)
    throw new Error("Declaration restoration changed the qualified bytes");
  requirePass("build-umbrella", process.execPath, [
    "scripts/build-umbrella.mjs",
  ]);
  const env = {
    ...environment,
    AGENTKIT_NATIVE_PACK_SKIP_BUILD: "1",
    AGENTKIT_PACKAGE_GATE_OUTPUT: join(output, "installed"),
  };
  requirePass(
    "installed-native",
    process.execPath,
    ["rust/scripts/check-native-package.mjs"],
    { env },
  );
  requirePass(
    "installed-umbrella",
    process.execPath,
    ["rust/scripts/check-native-umbrella.mjs"],
    { env },
  );
  evidence.artifacts = artifactHashes(output);
  evidence.status = "PASS";
} catch (error) {
  evidence.status = ["BLOCKED", "INCONCLUSIVE"].includes(
    commands.at(-1)?.status,
  )
    ? commands.at(-1).status
    : "FAIL";
  evidence.error = error instanceof Error ? error.message : String(error);
  process.stderr.write(`${evidence.error}\n`);
  process.exitCode = 1;
} finally {
  if (existsSync(join(output, "source-manifest.json"))) {
    const manifest = JSON.parse(
      readFileSync(join(output, "source-manifest.json"), "utf8"),
    );
    evidence.sourceDigest = manifest.sha256;
    evidence.buildSourceChanges = manifest.files.flatMap((file) => {
      const built = join(source, file.path);
      const after = existsSync(built) ? hash(readFileSync(built)) : null;
      return after === file.sha256
        ? []
        : [{ path: file.path, before: file.sha256, after }];
    });
  }
  evidence.artifacts = artifactHashes(output);
  json(join(output, "qualification.json"), evidence);
}
