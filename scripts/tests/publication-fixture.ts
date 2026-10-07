import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { sha256 } from "../release/foundation.mjs";
import { expectedPublication } from "../release/publication.mjs";

const sourceNames = [
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
const packageNames = [
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

export function writeJson(
  directory: string,
  file: string,
  value: unknown,
): void {
  writeFileSync(join(directory, file), `${JSON.stringify(value, null, 2)}\n`);
}

function tar(source: string, output: string, names: string[]): void {
  const result = spawnSync("tar", ["-czf", output, "-C", source, ...names]);
  if (result.status !== 0) throw new Error(result.stderr.toString());
}

function runtime(processReopened: boolean): object {
  return {
    electron: "41.6.1",
    node: "24.15.0",
    modules: "145",
    napi: "10",
    platform: "darwin",
    reopened: true,
    processReopened,
    nativePackage: { version: "13.0.3" },
    binaries: [{ path: "/fixture/driver.node", sha256: "a".repeat(64) }],
  };
}

function sourceProof(
  base: string,
  directory: string,
  commit: string,
): {
  files: { path: string; sha256: string }[];
  sha256: string;
  algorithm: string;
} {
  const source = join(base, "source");
  mkdirSync(source);
  const provenance = {
    format: "agentkit-source-projection-v1",
    release: "foundation",
    version: "0.6.0",
    baseline: commit,
    dirtyPatchSha256: sha256(""),
    patches: [],
    originalFiles: [],
  };
  writeJson(source, "snapshot-provenance.json", provenance);
  writeFileSync(join(source, "code.ts"), "export const fixture = true;\n");
  const files = ["code.ts", "snapshot-provenance.json"].map((path) => ({
    path,
    sha256: sha256(readFileSync(join(source, path))),
  }));
  const manifest = {
    algorithm: "sha256-sorted-source-manifest-v1",
    files,
    sha256: sha256(JSON.stringify(files)),
  };
  tar(
    source,
    join(directory, "source.tar.gz"),
    files.map((file) => file.path),
  );
  writeFileSync(
    join(directory, "snapshot-provenance.json"),
    readFileSync(join(source, "snapshot-provenance.json")),
  );
  writeFileSync(join(directory, "dirty.patch"), "");
  writeJson(directory, "source-manifest.json", manifest);
  writeJson(directory, "source-manifest-after.json", manifest);
  return manifest;
}

function qualificationProof(
  directory: string,
  digest: string,
  sourceDigest: string,
): object {
  const native = {
    nativePackage: { version: "13.0.3" },
    processReopened: true,
  };
  const qualification = {
    sha256: digest,
    sourceDigest,
    package: { name: "agentkit", version: "0.6.0" },
    passed: true,
    sqliteVersion: "13.0.3",
    npmNativeRuntime: native,
    bunNativeRuntime: native,
    checks: packageNames.map((name) => ({ name, status: 0, passed: true })),
    electronRuntimeQualified: true,
    electron: {
      before: runtime(false),
      after: runtime(true),
      mode: "ELECTRON_RUN_AS_NODE=1",
    },
  };
  mkdirSync(join(directory, "qualification"));
  writeJson(
    join(directory, "qualification"),
    "qualification.json",
    qualification,
  );
  tar(directory, join(directory, "qualification-evidence.tar.gz"), [
    "qualification",
  ]);
  return qualification;
}

export function publicationFixture(base: string): {
  directory: string;
  expected: ReturnType<typeof expectedPublication>;
} {
  const directory = join(base, "foundation");
  const commit = "b".repeat(40);
  mkdirSync(directory);
  const manifest = sourceProof(base, directory, commit);
  const packed = join(base, "package");
  mkdirSync(packed);
  writeJson(packed, "package.json", { name: "agentkit", version: "0.6.0" });
  tar(base, join(directory, "agentkit-0.6.0.tgz"), ["package"]);
  const digest = sha256(readFileSync(join(directory, "agentkit-0.6.0.tgz")));
  qualificationProof(directory, digest, manifest.sha256);
  writeJson(directory, "source-gates.json", {
    release: "foundation",
    version: "0.6.0",
    sourceOnly: false,
    checks: sourceNames.map((name) => ({ name, status: 0, passed: true })),
  });
  writeJson(directory, "candidate.json", {
    format: "agentkit-qualified-candidate-v1",
    version: "0.6.0",
    release: "foundation",
    track: "foundation",
    sourceCommit: commit,
    sourceRef: "refs/heads/master",
    qualificationRun: "10",
    sourceDigest: manifest.sha256,
    tarball: "agentkit-0.6.0.tgz",
    tarballSha256: digest,
    platform: "darwin",
    node: "v22.23.2",
    bun: "1.4.0",
    electron: "41.6.1",
    passed: true,
    sourceArchiveSha256: sha256(readFileSync(join(directory, "source.tar.gz"))),
    evidenceArchiveSha256: sha256(
      readFileSync(join(directory, "qualification-evidence.tar.gz")),
    ),
  });
  const expected = expectedPublication({
    QUALIFICATION_RUN: "10",
    CANDIDATE_ARTIFACT: "20",
    VERSION: "0.6.0",
    SOURCE_COMMIT: commit,
    SOURCE_DIGEST: manifest.sha256,
    TARBALL_SHA256: digest,
    GITHUB_REPOSITORY: "andrejvysny/AgentKit",
    DEFAULT_BRANCH: "master",
  });
  return { directory, expected };
}
