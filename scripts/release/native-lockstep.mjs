/**
 * Native-stack version lockstep for a release snapshot.
 *
 * The native npm stack — the `@agentkit/native` wrapper, every platform
 * package, and every dependency pin that points at them (wrapper →
 * platforms, umbrella → platforms, `@agentkit/core` → wrapper) — must ship
 * ONE version, and the committed NAPI-RS loader must embed exactly that
 * version (it compares each platform package's `package.json.version`
 * against the embedded string). A partial bump ships an installer that
 * cannot resolve the platform packages, or a loader whose version check is
 * vacuous — so the release projection rewrites all of it atomically or
 * fails.
 *
 * Rewrites (inside the snapshot directory only):
 *   packages/native/package.json                    version + platform pins
 *   packages/native-<platform>/package.json         version (×N, per table)
 *   packages/agentkit/package.json                  platform pins
 *   packages/core/package.json                      wrapper pin
 *   packages/native/generated/index.js              embedded version strings
 *   bun.lock                                        entry versions + spec pins
 *
 * `packages/native/tests/distribution.test.ts` enforces the same equality
 * for the dev tree; this module enforces it for the projected candidate.
 */
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";

function readJson(output, path) {
  return JSON.parse(readFileSync(join(output, path), "utf8"));
}

function writeJson(output, path, manifest) {
  writeFileSync(join(output, path), `${JSON.stringify(manifest, null, 2)}\n`);
}

/**
 * Bump the native stack in `output` (a snapshot-release copy of the tree)
 * to `version`. Returns the list of paths changed.
 */
export function bumpNativeStackVersions(output, version) {
  const changed = [];

  // Single dev version everywhere, or nothing moves (a half-bumped dev tree
  // is a bug in the dev tree, caught there by distribution.test.ts).
  const wrapper = readJson(output, "packages/native/package.json");
  const devVersion = String(wrapper.version);
  for (const target of NATIVE_PLATFORM_TARGETS) {
    const entry = readJson(output, `packages/${target.package}/package.json`);
    if (String(entry.version) !== devVersion)
      throw new Error(
        `native lockstep: packages/${target.package} is at ${entry.version}, wrapper at ${devVersion}`,
      );
  }

  // Wrapper: its own version + exact pins to each platform package.
  wrapper.version = version;
  for (const name of Object.keys(wrapper.optionalDependencies ?? {}))
    wrapper.optionalDependencies[name] = version;
  const wrapperKeys = Object.keys(wrapper.optionalDependencies ?? {}).sort();
  const expectedKeys = NATIVE_PLATFORM_TARGETS.map(
    (t) => `@agentkit/native-${t.key}`,
  ).sort();
  if (JSON.stringify(wrapperKeys) !== JSON.stringify(expectedKeys))
    throw new Error(
      `native lockstep: wrapper optionalDependencies ${wrapperKeys.join(",")} do not match the platform table ${expectedKeys.join(",")}`,
    );
  writeJson(output, "packages/native/package.json", wrapper);
  changed.push("packages/native/package.json");

  for (const target of NATIVE_PLATFORM_TARGETS) {
    const path = `packages/${target.package}/package.json`;
    const entry = readJson(output, path);
    entry.version = version;
    writeJson(output, path, entry);
    changed.push(path);
  }

  // Umbrella: exact pins to each platform package (its own version is
  // bumped by the caller).
  const umbrellaPath = "packages/agentkit/package.json";
  const umbrella = readJson(output, umbrellaPath);
  for (const target of NATIVE_PLATFORM_TARGETS)
    umbrella.optionalDependencies[`@agentkit/native-${target.key}`] = version;
  writeJson(output, umbrellaPath, umbrella);
  changed.push(umbrellaPath);

  // Core: the pin that pulls the wrapper for `agentkit/core` consumers.
  const corePath = "packages/core/package.json";
  const core = readJson(output, corePath);
  if (core.optionalDependencies?.["@agentkit/native"] === undefined)
    throw new Error("native lockstep: core lost its @agentkit/native pin");
  core.optionalDependencies["@agentkit/native"] = version;
  writeJson(output, corePath, core);
  changed.push(corePath);

  // Committed loader: the embedded version literals (comparison + message
  // per platform arm).
  const loaderPath = "packages/native/generated/index.js";
  let loader = readFileSync(join(output, loaderPath), "utf8");
  const comparisons = [
    ...loader.matchAll(/bindingPackageVersion !== '([^']+)'/g),
  ];
  if (comparisons.length === 0)
    throw new Error("native lockstep: loader has no version comparisons");
  for (const match of comparisons)
    if (match[1] !== devVersion)
      throw new Error(
        `native lockstep: loader comparison against "${match[1]}" does not match the wrapper version "${devVersion}"`,
      );
  loader = loader.split(`'${devVersion}'`).join(`'${version}'`);
  loader = loader
    .split(`expected ${devVersion} but got`)
    .join(`expected ${version} but got`);
  if (loader.includes(devVersion))
    throw new Error(
      `native lockstep: loader still references the dev version "${devVersion}"`,
    );
  writeFileSync(join(output, loaderPath), loader);
  changed.push(loaderPath);

  // bun.lock: workspace entry versions and the exact dependency specs. The
  // release projection must stay installable with --frozen-lockfile.
  // Dependency specs bumped here are the concrete dev-version pins only:
  // wrapper→platforms (2 each: wrapper entry + umbrella entry), core→wrapper
  // (1). `workspace:*` specs (examples) stay workspace links and must not be
  // touched; resolution-table rows carry `workspace:` identities, not specs.
  let lock = readFileSync(join(output, "bun.lock"), "utf8");
  const bumpEntryVersion = (text, dir, name) => {
    const pattern = new RegExp(
      `("packages/${dir}": \\{\\s*"name": "${name.replace(
        "/",
        "\\/",
      )}",\\s*"version": ")([^"]+)`,
    );
    const matches = [...text.matchAll(new RegExp(pattern.source, "g"))];
    if (matches.length !== 1)
      throw new Error(
        `native lockstep: bun.lock entry for packages/${dir} found ${matches.length} times`,
      );
    if (matches[0][2] !== devVersion)
      throw new Error(
        `native lockstep: bun.lock packages/${dir} is at "${matches[0][2]}", expected the dev version "${devVersion}"`,
      );
    return text.replace(pattern, `$1${version}`);
  };
  lock = bumpEntryVersion(lock, "native", "@agentkit/native");
  for (const target of NATIVE_PLATFORM_TARGETS)
    lock = bumpEntryVersion(
      lock,
      target.package,
      `@agentkit/native-${target.key}`,
    );
  const bumpSpecs = (text, key, expected) => {
    const escaped = devVersion.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const spec = new RegExp(`("${key}": ")${escaped}"`, "g");
    const matches = [...text.matchAll(spec)];
    if (matches.length !== expected)
      throw new Error(
        `native lockstep: bun.lock concrete specs for ${key} found ${matches.length}, expected ${expected}`,
      );
    return text.replace(spec, `$1${version}"`);
  };
  for (const target of NATIVE_PLATFORM_TARGETS)
    lock = bumpSpecs(lock, `@agentkit/native-${target.key}`, 2);
  lock = bumpSpecs(lock, "@agentkit/native", 1);
  writeFileSync(join(output, "bun.lock"), lock);
  changed.push("bun.lock");

  return changed;
}
