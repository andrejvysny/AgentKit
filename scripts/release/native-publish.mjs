#!/usr/bin/env node
/**
 * Native npm publication for the AgentKit release stack.
 *
 *   node scripts/release/native-publish.mjs verify  <artifactsDir> <version> --commit <sha>
 *   node scripts/release/native-publish.mjs publish <artifactsDir> <version> --commit <sha> [--execute]
 *
 * `verify` is the gate the workflow runs unconditionally: the artifact set
 * must be COMPLETE (all five triples, one per key from the platform table),
 * every digest and byte count must match its manifest, every package must
 * carry the exact release version, the wrapper tarball must be byte-identical
 * across all five build jobs, the umbrella tarball must be present with the
 * matching version and platform pins, and every manifest must name the same
 * source commit. `publish` verifies again and then publishes the VERIFIED
 * tarball bytes (no repack): platforms → wrapper → umbrella, so any
 * installable moment has its platform binaries live. With `--execute` it
 * really publishes (`NPM_TOKEN` required, provenance on); without it, every
 * package goes through `npm publish --dry-run`. Already-published exact
 * versions are skipped, which makes a retry after a partial run complete
 * the set without ever replacing what the registry already holds.
 *
 * Scope ownership is a maintainer decision (see DEVELOPING.md "Publishing
 * the npm stack"): nothing here can run without an approved
 * `agentkit-native-publication` environment dispatch.
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";

function fail(message) {
  throw new Error(`native-publish: ${message}`);
}

const sha256 = (file) =>
  createHash("sha256").update(readFileSync(file)).digest("hex");

function tarballManifest(dir, entry) {
  const file = join(dir, entry.path);
  if (!existsSync(file)) fail(`artifact missing: ${file}`);
  if (sha256(file) !== entry.sha256) fail(`sha256 mismatch for ${file}`);
  if (readFileSync(file).byteLength !== entry.bytes)
    fail(`byte length mismatch for ${file}`);
  return { file, entry };
}

function packedJson(tarballPath) {
  const result = spawnSync(
    "tar",
    ["-xzOf", tarballPath, "package/package.json"],
    {
      encoding: "utf8",
      maxBuffer: 16 * 1024 * 1024,
    },
  );
  if (result.status !== 0) fail(`cannot read package.json from ${tarballPath}`);
  return JSON.parse(result.stdout);
}

export function verifyArtifacts(artifactsDir, version, commit) {
  if (!/^\d+\.\d+\.\d+$/.test(version ?? ""))
    fail(`version ${version} is not a plain semver release`);
  if (!/^[0-9a-f]{40}$/.test(commit ?? ""))
    fail(`commit ${commit} is not a full SHA`);
  const plan = {
    version,
    commit,
    platforms: [],
    wrapper: null,
    umbrella: null,
  };
  let wrapperSha;
  for (const target of NATIVE_PLATFORM_TARGETS) {
    const manifestPath = join(
      artifactsDir,
      `artifact-manifest.${target.key}.json`,
    );
    if (!existsSync(manifestPath))
      fail(`missing artifact manifest for ${target.triple} (${manifestPath})`);
    const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
    if (manifest.format !== "agentkit-native-artifact-v1")
      fail(`unexpected manifest format for ${target.key}`);
    if (manifest.triple !== target.triple)
      fail(`manifest ${target.key} claims triple ${manifest.triple}`);
    if (manifest.version !== version)
      fail(
        `artifact ${target.key} is at ${manifest.version}, release is ${version}`,
      );
    if (manifest.package !== `@agentkit/native-${target.key}`)
      fail(`artifact ${target.key} is package ${manifest.package}`);
    if (manifest.wrapperPackage !== "@agentkit/native")
      fail(`artifact ${target.key} wrapper package ${manifest.wrapperPackage}`);
    if (manifest.builtFrom?.commit !== commit)
      fail(
        `artifact ${target.key} was built from ${manifest.builtFrom?.commit}, not the release commit ${commit}`,
      );
    const platform = tarballManifest(artifactsDir, manifest.files.platform);
    const wrapper = tarballManifest(artifactsDir, manifest.files.wrapper);
    const platformManifest = packedJson(platform.file);
    if (platformManifest.version !== version)
      fail(`platform tarball ${target.key} version drift`);
    if (platformManifest.private !== undefined)
      fail(`platform tarball ${target.key} is still private`);
    if (platformManifest.publishConfig?.access !== "public")
      fail(`platform tarball ${target.key} lacks publishConfig.access=public`);
    if (wrapperSha && wrapper.entry.sha256 !== wrapperSha)
      fail(
        `wrapper tarball is not byte-identical across build jobs (${target.key} differs) — nondeterministic build`,
      );
    wrapperSha = wrapper.entry.sha256;
    plan.platforms.push({
      key: target.key,
      triple: target.triple,
      package: platform.file,
      wrapper: wrapper.file,
      structuralOnly: manifest.structuralOnly === true,
    });
    plan.wrapper = wrapper.file;
  }
  const umbrellaPath = join(artifactsDir, `agentkit-${version}.tgz`);
  if (!existsSync(umbrellaPath))
    fail(`missing umbrella tarball ${umbrellaPath}`);
  const umbrellaManifest = packedJson(umbrellaPath);
  if (umbrellaManifest.name !== "agentkit")
    fail(`umbrella tarball is ${umbrellaManifest.name}`);
  if (umbrellaManifest.version !== version)
    fail(
      `umbrella tarball is at ${umbrellaManifest.version}, release is ${version}`,
    );
  for (const target of NATIVE_PLATFORM_TARGETS)
    if (
      umbrellaManifest.optionalDependencies?.[
        `@agentkit/native-${target.key}`
      ] !== version
    )
      fail(
        `umbrella does not pin @agentkit/native-${target.key} at ${version}`,
      );
  if (JSON.stringify(umbrellaManifest).includes("workspace:"))
    fail("umbrella tarball contains a workspace: dependency range");
  const wrapperManifest = packedJson(plan.wrapper);
  if (wrapperManifest.version !== version)
    fail(
      `wrapper tarball is at ${wrapperManifest.version}, release is ${version}`,
    );
  if (wrapperManifest.publishConfig?.access !== "public")
    fail("wrapper tarball lacks publishConfig.access=public");
  plan.umbrella = umbrellaPath;
  return plan;
}

function npmViewExists(name, version) {
  const result = spawnSync("npm", ["view", `${name}@${version}`, "version"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (result.status === 0) return true;
  if (/E404|404/.test(`${result.stdout}${result.stderr}`)) return false;
  fail(`npm view ${name}@${version} failed: ${result.stderr || result.stdout}`);
}

function npmPublish(file, { execute }) {
  const args = ["publish", file, "--access", "public"];
  if (!execute) args.push("--dry-run");
  else args.push("--provenance");
  const result = spawnSync("npm", args, {
    encoding: "utf8",
    stdio: execute ? "inherit" : "pipe",
  });
  if (result.status !== 0) {
    if (!execute)
      process.stderr.write(`${result.stdout ?? ""}${result.stderr ?? ""}`);
    fail(`npm publish ${file} failed with status ${result.status}`);
  }
}

export function publishArtifacts(artifactsDir, version, commit, execute) {
  const plan = verifyArtifacts(artifactsDir, version, commit);
  if (execute && !process.env.NPM_TOKEN)
    fail(
      "publishing requires NPM_TOKEN (an automation token for the npm account owning the scope)",
    );
  if (execute) {
    // Registry auth via npm's own env expansion — the token never touches
    // argv, a committed file, or this process' logs.
    const npmrc = join(artifactsDir, "release.npmrc");
    // The ${NPM_TOKEN} placeholder is expanded by npm itself from the
    // process environment — never by this template and never into logs.
    // biome-ignore lint/suspicious/noTemplateCurlyInString: npm expands this placeholder at publish time
    writeFileSync(
      npmrc,
      "//registry.npmjs.org/:_authToken=${NPM_TOKEN}\nregistry=https://registry.npmjs.org/\n",
      { mode: 0o600 },
    );
    process.env.NPM_CONFIG_USERCONFIG = npmrc;
  }
  const targets = [
    ...plan.platforms.map((p) => ({
      file: p.package,
      manifest: packedJson(p.package),
    })),
    { file: plan.wrapper, manifest: packedJson(plan.wrapper) },
    { file: plan.umbrella, manifest: packedJson(plan.umbrella) },
  ];
  for (const target of targets) {
    const name = target.manifest.name;
    if (execute && npmViewExists(name, version)) {
      console.log(
        `native-publish: ${name}@${version} already published — skipping (partial-run resume)`,
      );
      continue;
    }
    console.log(
      `native-publish: ${execute ? "publishing" : "dry-run"} ${name}@${version}`,
    );
    npmPublish(target.file, { execute });
  }
  console.log(
    execute
      ? `native-publish: published agentkit@${version} + @agentkit/native@${version} + ${plan.platforms.length} platform packages.`
      : `native-publish: dry-run complete — ${targets.length} packages would publish at ${version}.`,
  );
  return plan;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const [command, artifactsDir, version, ...rest] = process.argv.slice(2);
  try {
    if (!["verify", "publish"].includes(command) || !artifactsDir || !version)
      fail(
        "usage: node scripts/release/native-publish.mjs verify|publish <artifactsDir> <version> --commit <sha> [--execute]",
      );
    const commitFlag = rest.indexOf("--commit");
    const commit = commitFlag >= 0 ? rest[commitFlag + 1] : undefined;
    if (command === "verify") {
      const plan = verifyArtifacts(artifactsDir, version, commit);
      console.log(
        `native-publish: verified ${plan.platforms.length} platform artifacts + wrapper + umbrella at ${version} from ${commit}` +
          (plan.platforms.some((p) => p.structuralOnly)
            ? ` (${plan.platforms
                .filter((p) => p.structuralOnly)
                .map((p) => p.triple)
                .join(", ")} build-verified only)`
            : ""),
      );
    } else {
      publishArtifacts(
        artifactsDir,
        version,
        commit,
        rest.includes("--execute"),
      );
    }
  } catch (error) {
    console.error(String(error?.message ?? error));
    process.exit(1);
  }
}
