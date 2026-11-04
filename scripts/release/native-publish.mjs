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
import { basename, isAbsolute, join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { assertBinaryBytesMatchTarget } from "../../rust/scripts/build-native-release-artifact.mjs";
import { NATIVE_PLATFORM_TARGETS } from "../../rust/scripts/native-platform-targets.mjs";

function fail(message) {
  throw new Error(`native-publish: ${message}`);
}

const sha256 = (file) =>
  createHash("sha256").update(readFileSync(file)).digest("hex");

const sha512Base64 = (file) =>
  createHash("sha512").update(readFileSync(file)).digest("base64");

function tarballManifest(dir, entry, expectedBasename) {
  const file = resolve(dir, entry.path);
  // The manifest is JSON the build job wrote; a path that escapes the
  // artifact directory (or names an unexpected tarball) is not an
  // artifact, whatever digest it carries.
  const inside = relative(resolve(dir), file);
  if (
    inside.startsWith("..") ||
    isAbsolute(inside) ||
    basename(file) !== expectedBasename
  )
    fail(
      `artifact path ${entry.path} is not the expected ${expectedBasename} inside the artifact dir`,
    );
  if (!existsSync(file)) fail(`artifact missing: ${file}`);
  if (sha256(file) !== entry.sha256) fail(`sha256 mismatch for ${file}`);
  if (readFileSync(file).byteLength !== entry.bytes)
    fail(`byte length mismatch for ${file}`);
  return { file, entry };
}

function tarMember(tarballPath, member, { encoding = "utf8" } = {}) {
  const result = spawnSync("tar", ["-xzOf", tarballPath, member], {
    encoding,
    maxBuffer: 256 * 1024 * 1024,
  });
  if (result.status !== 0) fail(`cannot read ${member} from ${tarballPath}`);
  return result.stdout;
}

function packedJson(tarballPath) {
  const stdout = tarMember(tarballPath, "package/package.json");
  try {
    return JSON.parse(stdout);
  } catch {
    fail(`package.json in ${tarballPath} is not valid JSON`);
  }
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
  let cargoLock;
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
    if (typeof manifest.builtFrom?.cargoLockSha256 !== "string")
      fail(`artifact ${target.key} lacks Cargo.lock provenance`);
    if (cargoLock && manifest.builtFrom.cargoLockSha256 !== cargoLock)
      fail(
        `artifact ${target.key} was built against a different Cargo.lock than the other triples — the snapshot is not one frozen source`,
      );
    cargoLock = manifest.builtFrom.cargoLockSha256;
    const platform = tarballManifest(
      artifactsDir,
      manifest.files.platform,
      `agentkit-native-${target.key}-${version}.tgz`,
    );
    const wrapper = tarballManifest(
      artifactsDir,
      manifest.files.wrapper,
      `agentkit-native-${version}.tgz`,
    );
    // Everything the registry will serve, re-derived from the tarball
    // bytes — never taken from the build job's own manifest claims.
    const platformManifest = packedJson(platform.file);
    if (platformManifest.name !== `@agentkit/native-${target.key}`)
      fail(
        `platform tarball ${target.key} is package ${platformManifest.name}`,
      );
    if (platformManifest.version !== version)
      fail(`platform tarball ${target.key} version drift`);
    if (platformManifest.private !== undefined)
      fail(`platform tarball ${target.key} is still private`);
    if (platformManifest.publishConfig?.access !== "public")
      fail(`platform tarball ${target.key} lacks publishConfig.access=public`);
    if (JSON.stringify(platformManifest.os) !== JSON.stringify([target.os]))
      fail(
        `platform tarball ${target.key} os gate ${JSON.stringify(platformManifest.os)} != [${target.os}]`,
      );
    if (JSON.stringify(platformManifest.cpu) !== JSON.stringify([target.cpu]))
      fail(
        `platform tarball ${target.key} cpu gate ${JSON.stringify(platformManifest.cpu)} != [${target.cpu}]`,
      );
    if (
      JSON.stringify(platformManifest.libc) !==
      JSON.stringify(target.libc ? [target.libc] : undefined)
    )
      fail(
        `platform tarball ${target.key} libc gate ${JSON.stringify(platformManifest.libc)} != ${String(target.libc)}`,
      );
    const addonMember = `package/agentkit-native.${target.key}.node`;
    if (platformManifest.main !== `agentkit-native.${target.key}.node`)
      fail(
        `platform tarball ${target.key} main ${String(platformManifest.main)}`,
      );
    // The machine code inside the tarball must BE the claimed triple's —
    // the publish gate never trusts a filename or the build job's word.
    const packedBytes = tarMember(platform.file, addonMember, {
      encoding: null,
    });
    if (!packedBytes || packedBytes.length === 0)
      fail(`platform tarball ${target.key} does not contain ${addonMember}`);
    assertBinaryBytesMatchTarget(
      packedBytes,
      target,
      `${basename(platform.file)}:${addonMember}`,
    );
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
  if (wrapperManifest.name !== "@agentkit/native")
    fail(`wrapper tarball is ${wrapperManifest.name}`);
  if (wrapperManifest.version !== version)
    fail(
      `wrapper tarball is at ${wrapperManifest.version}, release is ${version}`,
    );
  if (wrapperManifest.publishConfig?.access !== "public")
    fail("wrapper tarball lacks publishConfig.access=public");
  for (const target of NATIVE_PLATFORM_TARGETS)
    if (
      wrapperManifest.optionalDependencies?.[
        `@agentkit/native-${target.key}`
      ] !== version
    )
      fail(
        `wrapper tarball does not pin @agentkit/native-${target.key} at ${version} — a released wrapper must resolve exactly its own platform builds`,
      );
  plan.umbrella = umbrellaPath;
  return plan;
}

function npmRegistryIntegrity(npm, name, version) {
  // --registry on the command line outranks every .npmrc (including a
  // repo-committed project .npmrc), so no source commit can redirect the
  // probe — or the publish below — to a lookalike registry.
  const result = spawnSync(
    npm,
    [
      "view",
      `${name}@${version}`,
      "dist.integrity",
      "--registry",
      "https://registry.npmjs.org/",
    ],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  if (result.status !== 0) {
    if (/E404|404/.test(`${result.stdout}${result.stderr}`)) return null;
    fail(
      `npm view ${name}@${version} failed: ${result.stderr || result.stdout}`,
    );
  }
  return result.stdout.trim();
}

function npmPublish(npm, file, { execute }) {
  const args = [
    "publish",
    file,
    "--access",
    "public",
    "--registry",
    "https://registry.npmjs.org/",
  ];
  if (!execute) args.push("--dry-run");
  else args.push("--provenance");
  const result = spawnSync(npm, args, {
    encoding: "utf8",
    stdio: execute ? "inherit" : "pipe",
  });
  if (result.status !== 0) {
    if (!execute)
      process.stderr.write(`${result.stdout ?? ""}${result.stderr ?? ""}`);
    fail(`npm publish ${file} failed with status ${result.status}`);
  }
}

/**
 * `npm` is the npm executable to drive. Tests pass an absolute path to a
 * fake: a PATH mutation is not honoured by every runtime's spawn lookup
 * (Bun 1.3 resolves against the startup PATH), and a missed fake would
 * reach the real registry.
 */
export function publishArtifacts(
  artifactsDir,
  version,
  commit,
  execute,
  { npm = "npm" } = {},
) {
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
    writeFileSync(
      npmrc,
      // biome-ignore lint/suspicious/noTemplateCurlyInString: npm expands this placeholder at publish time
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
    if (execute) {
      const registryIntegrity = npmRegistryIntegrity(npm, name, version);
      if (registryIntegrity !== null) {
        // Skip a completed publish only when the registry holds EXACTLY
        // these verified bytes — an unpublish-and-replace of the same
        // version must fail loudly, never resume past a substitution.
        const local = `sha512-${sha512Base64(target.file)}`;
        if (registryIntegrity !== local)
          fail(
            `${name}@${version} exists in the registry with different bytes (registry ${registryIntegrity}, verified artifact ${local}) — refusing to skip`,
          );
        console.log(
          `native-publish: ${name}@${version} already published byte-identically — skipping (partial-run resume)`,
        );
        continue;
      }
    }
    console.log(
      `native-publish: ${execute ? "publishing" : "dry-run"} ${name}@${version}`,
    );
    npmPublish(npm, target.file, { execute });
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
