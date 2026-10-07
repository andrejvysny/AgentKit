import { spawnSync } from "node:child_process";
import { lstatSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sha256 } from "./foundation.mjs";

export function requireProof(condition, message) {
  if (!condition) throw new Error(`Publication refused: ${message}`);
}

function archiveNames(archive, manifest) {
  const listing = spawnSync("tar", ["-tzf", archive], { encoding: "utf8" });
  const types = spawnSync("tar", ["-tvzf", archive], { encoding: "utf8" });
  const names = listing.stdout.trimEnd().split("\n");
  requireProof(
    listing.status === 0 &&
      types.status === 0 &&
      types.stdout
        .trimEnd()
        .split("\n")
        .every((line) => line.startsWith("-")),
    "source archive contains nonregular entries",
  );
  const expected = new Set(manifest.files.map((file) => file.path));
  requireProof(
    names.length === expected.size &&
      new Set(names).size === names.length &&
      names.every(
        (name) =>
          !name.startsWith("/") &&
          !name.includes("\\") &&
          name
            .split("/")
            .every((part) => part !== ".." && part !== "." && part !== "") &&
          expected.has(name),
      ),
    "source archive paths differ from manifest",
  );
}

export function verifySourceArchive(archive, manifest) {
  archiveNames(archive, manifest);
  const directory = mkdtempSync(join(tmpdir(), "agentkit-source-proof-"));
  try {
    const result = spawnSync("tar", [
      "-xzf",
      archive,
      "-C",
      directory,
      "--no-same-owner",
      "--no-same-permissions",
    ]);
    requireProof(result.status === 0, "source archive extraction failed");
    for (const file of manifest.files)
      requireProof(
        lstatSync(join(directory, file.path)).isFile() &&
          sha256(readFileSync(join(directory, file.path))) === file.sha256,
        `frozen source differs: ${file.path}`,
      );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}
