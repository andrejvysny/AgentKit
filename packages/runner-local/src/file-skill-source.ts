import { constants, type Stats } from "node:fs";
import { lstat, open, readdir, realpath } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";
import type { SkillSource } from "@agentkit/host";

const MAX_RESOURCE_BYTES = 1_000_000;

function inside(root: string, target: string): void {
  const rel = relative(root, target);
  if (isAbsolute(rel) || rel === ".." || rel.startsWith(`..${sep}`))
    throw new Error("Skill path escapes configured root.");
}

function sameFile(left: Stats, right: Stats): boolean {
  return (
    left.dev === right.dev &&
    left.ino === right.ino &&
    left.size === right.size &&
    left.mtimeMs === right.mtimeMs &&
    left.ctimeMs === right.ctimeMs
  );
}

async function verifyPath(
  root: string,
  skillRoot: string,
  target: string,
  expected: Stats,
): Promise<void> {
  const currentRoot = await realpath(root);
  const currentSkill = await realpath(skillRoot);
  const currentTarget = await realpath(target);
  inside(root, currentRoot);
  inside(root, currentSkill);
  inside(skillRoot, currentTarget);
  if (
    currentRoot !== root ||
    currentSkill !== skillRoot ||
    currentTarget !== target ||
    !sameFile(expected, await lstat(target))
  )
    throw new Error("Skill resource changed during read.");
}

async function boundedRead(
  root: string,
  skillRoot: string,
  target: string,
  signal?: AbortSignal,
): Promise<string> {
  const expected = await lstat(target);
  if (!expected.isFile() || expected.size > MAX_RESOURCE_BYTES)
    throw new Error("Skill resource is not a bounded regular file.");
  const handle = await open(
    target,
    constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
  );
  try {
    const opened = await handle.stat();
    if (!opened.isFile() || !sameFile(expected, opened))
      throw new Error("Skill resource changed during open.");
    await verifyPath(root, skillRoot, target, opened);
    const bytes = Buffer.alloc(MAX_RESOURCE_BYTES + 1);
    let total = 0;
    for (;;) {
      signal?.throwIfAborted();
      const { bytesRead } = await handle.read(
        bytes,
        total,
        bytes.length - total,
        null,
      );
      if (!bytesRead) break;
      total += bytesRead;
      if (total > MAX_RESOURCE_BYTES)
        throw new Error("Skill resource exceeds byte limit.");
    }
    await verifyPath(root, skillRoot, target, opened);
    signal?.throwIfAborted();
    return bytes.subarray(0, total).toString("utf8");
  } finally {
    await handle.close();
  }
}

async function listSkillPaths(
  root: string,
  signal?: AbortSignal,
): Promise<string[]> {
  if ((await realpath(root)) !== root)
    throw new Error("Skill path escapes configured root.");
  const entries = await readdir(root, { withFileTypes: true });
  if (entries.length > 1000)
    throw new Error("Skill directory exceeds entry limit.");
  if (entries.some((entry) => entry.name === "SKILL.md" && entry.isFile()))
    return ["."];
  const result: string[] = [];
  for (const entry of entries) {
    signal?.throwIfAborted();
    if (!entry.isDirectory() || entry.isSymbolicLink()) continue;
    try {
      const skillRoot = await realpath(resolve(root, entry.name));
      inside(root, skillRoot);
      inside(skillRoot, await realpath(resolve(skillRoot, "SKILL.md")));
      result.push(entry.name);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
  }
  signal?.throwIfAborted();
  return result.sort();
}

/**
 * A selected root only; no automatic home-directory or ancestor discovery.
 * Descriptor identity and path rechecks detect replacement around open/read.
 * Portable Node APIs cannot atomically resolve every ancestor: use an OS sandbox
 * when an adversary can rename directories concurrently with the host.
 */
export function fileSkillSource(
  id: string,
  configuredRoot: string,
): SkillSource {
  let pinnedRoot: Promise<string> | undefined;
  const rootPath = () => (pinnedRoot ??= realpath(configuredRoot));
  return {
    id,
    async list(signal) {
      signal?.throwIfAborted();
      const root = await rootPath();
      return listSkillPaths(root, signal);
    },
    async read(skill, path, signal) {
      signal?.throwIfAborted();
      const root = await rootPath();
      const skillRoot = await realpath(resolve(root, skill));
      inside(root, skillRoot);
      if (
        isAbsolute(path) ||
        path.includes("\\") ||
        path.split("/").includes("..")
      )
        throw new Error("Invalid skill reference.");
      const target = await realpath(resolve(skillRoot, path));
      inside(skillRoot, target);
      return boundedRead(root, skillRoot, target, signal);
    },
  };
}

// Same `./filesystem` subpath: both read configured roots with the same
// confinement rules.
export { fileProjectInstructions } from "./project-instructions.js";
