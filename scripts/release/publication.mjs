import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { sha256 } from "./foundation.mjs";
import { requireProof, verifySourceArchive } from "./source-proof.mjs";

const sourceChecks = [
  "snapshot",
  "source-before",
  "source-archive",
  "install",
  "typecheck",
  "lint",
  "tests",
  "build",
  "umbrella",
  "node-smoke",
  "node-sqlite",
  "bun-sqlite",
  "source-after",
  "pack",
  "package-qualification",
  "evidence-archive",
];
const packageChecks = [
  "npm-version",
  "bun-version",
  "artifact-manifest",
  "npm-install",
  "npm-node-import-bundle-native-host",
  "npm-types",
  "npm-node-native-seed",
  "npm-node-native-process-reopen",
  "bun-install",
  "bun-node-import-bundle-native-host",
  "bun-types",
  "bun-node-native-seed",
  "bun-node-native-process-reopen",
  "bun-sqlite",
  "electron-install",
  "electron-consumer-node-bundle",
  "electron-binary-install",
  "electron-native-before-rebuild",
  "electron-npm-rebuild",
  "electron-native-after-rebuild",
];

export function expectedPublication(environment) {
  const expected = {
    run: environment.QUALIFICATION_RUN,
    artifact: environment.CANDIDATE_ARTIFACT,
    version: environment.VERSION,
    commit: environment.SOURCE_COMMIT,
    sourceDigest: environment.SOURCE_DIGEST,
    tarballDigest: environment.TARBALL_SHA256,
    repository: environment.GITHUB_REPOSITORY,
    defaultBranch: environment.DEFAULT_BRANCH,
    track: environment.VERSION === "0.6.0" ? "foundation" : "responses",
  };
  requireProof(/^\d+$/.test(expected.run ?? ""), "invalid qualification run");
  requireProof(/^\d+$/.test(expected.artifact ?? ""), "invalid artifact ID");
  requireProof(
    ["0.6.0", "0.7.0"].includes(expected.version),
    "unsupported candidate version",
  );
  requireProof(
    /^[a-f0-9]{40}$/.test(expected.commit ?? ""),
    "invalid source commit",
  );
  for (const value of [expected.sourceDigest, expected.tarballDigest])
    requireProof(
      /^[a-f0-9]{64}$/.test(value ?? ""),
      "invalid expected SHA-256",
    );
  requireProof(
    /^[\w.-]+\/[\w.-]+$/.test(expected.repository ?? ""),
    "invalid repository",
  );
  requireProof(
    Boolean(expected.defaultBranch),
    "missing trusted default branch",
  );
  return expected;
}

export function verifyRun(run, artifact, expected) {
  requireProof(String(run.id) === expected.run, "wrong qualification run");
  requireProof(
    run.status === "completed" && run.conclusion === "success",
    "qualification did not succeed",
  );
  requireProof(
    run.event === "workflow_dispatch",
    "qualification must be manually dispatched",
  );
  requireProof(
    run.path === ".github/workflows/release.yml",
    "wrong qualification workflow",
  );
  requireProof(run.head_sha === expected.commit, "source commit differs");
  requireProof(
    run.head_branch === expected.defaultBranch,
    "source is not trusted default branch",
  );
  requireProof(
    run.repository?.full_name === expected.repository &&
      run.head_repository?.full_name === expected.repository,
    "qualification came from another repository",
  );
  const name = `agentkit-candidates-${expected.track}-${expected.commit}-macos-latest`;
  requireProof(
    String(artifact.id) === expected.artifact &&
      artifact.name === name &&
      !artifact.expired,
    "wrong or expired macOS artifact",
  );
  requireProof(
    artifact.workflow_run?.id === run.id &&
      artifact.workflow_run?.head_sha === expected.commit,
    "artifact is not from the qualified source run",
  );
  return name;
}

function json(directory, name) {
  return JSON.parse(readFileSync(join(directory, name), "utf8"));
}

function successfulChecks(checks, names) {
  requireProof(
    Array.isArray(checks) &&
      checks.length > 0 &&
      checks.every((check) => check.status === 0 && check.passed === true),
    "failed or missing checks",
  );
  for (const name of names)
    requireProof(
      checks.filter((check) => check.name === name).length === 1,
      `missing or duplicate ${name}`,
    );
}

function verifyElectron(qualification) {
  requireProof(
    qualification.electronRuntimeQualified === true &&
      qualification.sqliteVersion === "13.0.3",
    "native combination was not qualified",
  );
  requireProof(
    qualification.electron?.mode === "ELECTRON_RUN_AS_NODE=1",
    "wrong Electron qualification mode",
  );
  for (const runtime of [
    qualification.electron.before,
    qualification.electron.after,
  ]) {
    requireProof(
      runtime.electron === "41.6.1" &&
        runtime.node === "24.15.0" &&
        runtime.modules === "145" &&
        runtime.napi === "10" &&
        runtime.platform === "darwin" &&
        runtime.nativePackage?.version === "13.0.3" &&
        runtime.reopened === true,
      "unexpected Electron/Node/native runtime",
    );
    requireProof(
      Array.isArray(runtime.binaries) &&
        runtime.binaries.length > 0 &&
        runtime.binaries.every((binary) =>
          /^[a-f0-9]{64}$/.test(binary.sha256),
        ),
      "missing loaded native binary digests",
    );
  }
  requireProof(
    qualification.electron.after.processReopened === true,
    "Electron process reopen did not pass",
  );
}

function verifyIdentity(proof, expected) {
  const { candidate, provenance, gates, release } = proof;
  requireProof(
    candidate.format === "agentkit-qualified-candidate-v1" &&
      candidate.passed === true &&
      candidate.version === expected.version &&
      candidate.release === release &&
      candidate.track === expected.track &&
      candidate.qualificationRun === expected.run &&
      candidate.sourceRef === `refs/heads/${expected.defaultBranch}`,
    "wrong candidate identity or qualification track",
  );
  requireProof(
    candidate.sourceCommit === expected.commit &&
      provenance.baseline === expected.commit &&
      provenance.version === expected.version &&
      provenance.release === release &&
      provenance.format === "agentkit-source-projection-v1",
    "wrong source provenance",
  );
  requireProof(
    candidate.node === "v22.23.2" &&
      candidate.bun === "1.4.0" &&
      candidate.electron === "41.6.1" &&
      candidate.platform === "darwin",
    "unexpected candidate runtimes",
  );
  requireProof(
    gates.release === release &&
      gates.version === expected.version &&
      gates.sourceOnly === false,
    "wrong source gates",
  );
  successfulChecks(gates.checks, sourceChecks);
}

function verifyManifests(directory, proof, expected) {
  const { manifest, after, provenance, candidate } = proof;
  requireProof(
    manifest.algorithm === "sha256-sorted-source-manifest-v1" &&
      sha256(JSON.stringify(manifest.files)) === manifest.sha256 &&
      manifest.sha256 === expected.sourceDigest &&
      after.sha256 === manifest.sha256 &&
      sha256(JSON.stringify(after.files)) === after.sha256 &&
      candidate.sourceDigest === manifest.sha256,
    "source manifest digest differs",
  );
  const entry = manifest.files.find(
    (file) => file.path === "snapshot-provenance.json",
  );
  requireProof(
    entry?.sha256 ===
      sha256(readFileSync(join(directory, "snapshot-provenance.json"))),
    "provenance is not in frozen source manifest",
  );
  requireProof(
    readFileSync(join(directory, "dirty.patch")).length === 0 &&
      provenance.dirtyPatchSha256 === sha256(""),
    "publication requires clean Git source",
  );
  for (const [file, digestKey] of [
    ["source.tar.gz", "sourceArchiveSha256"],
    ["qualification-evidence.tar.gz", "evidenceArchiveSha256"],
  ])
    requireProof(
      sha256(readFileSync(join(directory, file))) === candidate[digestKey],
      `${file} digest differs`,
    );
  verifySourceArchive(join(directory, "source.tar.gz"), manifest);
}

function verifyPackage(directory, proof, expected, readPackedManifest) {
  const { candidate, qualification, manifest } = proof;
  const tarball = `agentkit-${expected.version}.tgz`;
  const digest = sha256(readFileSync(join(directory, tarball)));
  requireProof(
    candidate.tarball === tarball &&
      digest === expected.tarballDigest &&
      candidate.tarballSha256 === digest &&
      qualification.sha256 === digest &&
      qualification.sourceDigest === manifest.sha256 &&
      qualification.passed === true,
    "package bytes or qualification differ",
  );
  requireProof(
    qualification.package?.name === "agentkit" &&
      qualification.package.version === expected.version,
    "qualified package version differs",
  );
  const packed = readPackedManifest(join(directory, tarball));
  requireProof(
    packed.name === "agentkit" &&
      packed.version === expected.version &&
      !JSON.stringify(packed).includes("workspace:"),
    "packed manifest differs",
  );
  successfulChecks(qualification.checks, packageChecks);
  for (const manager of ["npm", "bun"])
    requireProof(
      qualification[`${manager}NativeRuntime`]?.processReopened === true &&
        qualification[`${manager}NativeRuntime`].nativePackage?.version ===
          "13.0.3",
      `${manager} Node native process reopen is missing`,
    );
  verifyElectron(qualification);
  return tarball;
}

function verifyMigration(directory, qualification) {
  successfulChecks(qualification.checks, [
    "npm-responses-reopen",
    "npm-responses-openpcb-reopen",
    "bun-responses-reopen",
    "bun-responses-openpcb-reopen",
    "migration-a-install",
    "migration-a-seed",
    "migration-b-responses-reopen",
  ]);
  const foundation = json(join(directory, "../foundation"), "candidate.json");
  const migration = qualification.migration;
  requireProof(
    migration?.from === 8 &&
      migration.to === 9 &&
      migration.populated === true &&
      migration.reopened === true &&
      migration.sha256 === foundation.tarballSha256 &&
      migration.sha256 ===
        sha256(
          readFileSync(join(directory, "../foundation/agentkit-0.6.0.tgz")),
        ),
    "exact populated foundation migration is missing",
  );
}

export function verifyCandidate(
  directory,
  expected,
  readPackedManifest = packedManifest,
) {
  const proof = {
    candidate: json(directory, "candidate.json"),
    manifest: json(directory, "source-manifest.json"),
    after: json(directory, "source-manifest-after.json"),
    provenance: json(directory, "snapshot-provenance.json"),
    gates: json(directory, "source-gates.json"),
    qualification: json(join(directory, "qualification"), "qualification.json"),
    release: expected.version === "0.6.0" ? "foundation" : "responses",
  };
  verifyIdentity(proof, expected);
  verifyManifests(directory, proof, expected);
  const tarball = verifyPackage(directory, proof, expected, readPackedManifest);
  if (proof.release === "responses")
    verifyMigration(directory, proof.qualification);
  return { candidate: proof.candidate, tarball, tag: `v${expected.version}` };
}

function packedManifest(tarball) {
  const listing = spawnSync("tar", ["-tzf", tarball], { encoding: "utf8" });
  requireProof(
    listing.status === 0 &&
      listing.stdout
        .split("\n")
        .filter((path) => path === "package/package.json").length === 1,
    "packed manifest is missing or duplicated",
  );
  const result = spawnSync("tar", ["-xzOf", tarball, "package/package.json"], {
    encoding: "utf8",
  });
  requireProof(result.status === 0, "cannot read packed manifest");
  return JSON.parse(result.stdout);
}

function api(path) {
  const result = spawnSync("gh", ["api", path], { encoding: "utf8" });
  if (result.status !== 0)
    throw new Error(`GitHub verification failed: ${result.stderr}`);
  return JSON.parse(result.stdout);
}

function requireAbsent(path) {
  const result = spawnSync("gh", ["api", "--include", path], {
    encoding: "utf8",
  });
  requireProof(
    result.status !== 0 && /^HTTP\/\S+ 404\b/m.test(result.stdout),
    `tag or release already exists, or lookup failed: ${path}`,
  );
}

export function verifyJobs(jobs) {
  for (const name of ["bun-minimum", "acceptance"])
    requireProof(
      jobs.filter((job) => job.name === name && job.conclusion === "success")
        .length === 1,
      `${name} did not succeed`,
    );
  const platforms = jobs.filter((job) =>
    job.name.startsWith("qualification ("),
  );
  requireProof(
    platforms.length === 2 &&
      platforms.every((job) => job.conclusion === "success") &&
      platforms.some((job) => job.name.includes("macos-latest")) &&
      platforms.some((job) => job.name.includes("ubuntu-latest")),
    "platform qualification is missing",
  );
}

function verifyRemoteRun(expected) {
  const environment = api(
    `repos/${expected.repository}/environments/agentkit-publication`,
  );
  requireProof(
    environment.protection_rules?.some(
      (rule) =>
        rule.type === "required_reviewers" && rule.reviewers?.length > 0,
    ),
    "publication environment needs configured required reviewers",
  );
  const run = api(`repos/${expected.repository}/actions/runs/${expected.run}`);
  const artifact = api(
    `repos/${expected.repository}/actions/artifacts/${expected.artifact}`,
  );
  verifyRun(run, artifact, expected);
  verifyJobs(
    api(
      `repos/${expected.repository}/actions/runs/${expected.run}/jobs?per_page=100`,
    ).jobs,
  );
}

function prepareRelease(expected) {
  const directory = join(resolve(process.argv[3]), expected.track);
  const verified = verifyCandidate(directory, expected);
  requireAbsent(`repos/${expected.repository}/git/ref/tags/${verified.tag}`);
  requireAbsent(`repos/${expected.repository}/releases/tags/${verified.tag}`);
  writeFileSync(
    join(directory, "SHA256SUMS"),
    `${expected.tarballDigest}  ${verified.tarball}\n`,
  );
  writeFileSync(
    join(directory, "release-notes.md"),
    `Exact qualified ${expected.track} artifact ${expected.version}.\n\nSource commit: ${expected.commit}\nSource digest: ${expected.sourceDigest}\nTarball SHA-256: ${expected.tarballDigest}\nQualification run: https://github.com/${expected.repository}/actions/runs/${expected.run}\n\nInstall the attached tarball. The source tag alone is not an installable package. Electron 41.6.1 is qualified in RunAsNode mode; no GUI or source recompilation claim.\n`,
  );
  writeFileSync(
    process.env.GITHUB_OUTPUT,
    `candidate_directory=${directory}\ntag=${verified.tag}\ntarball=${verified.tarball}\n`,
    { flag: "a" },
  );
}

function main() {
  const expected = expectedPublication(process.env);
  requireProof(
    process.env.GITHUB_REF === `refs/heads/${expected.defaultBranch}`,
    "publisher must run on default branch",
  );
  if (process.argv[2] === "run") return verifyRemoteRun(expected);
  requireProof(process.argv[2] === "candidate", "unknown verification mode");
  prepareRelease(expected);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
)
  main();
