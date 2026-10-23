import { constants } from "node:fs";
import { open, readdir, realpath } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";
import type { SkillSource } from "@agentkit/host";

/** A selected root only; no automatic home-directory or ancestor discovery. */
export function fileSkillSource(
  id: string,
  configuredRoot: string,
): SkillSource {
  const inside = (root: string, target: string) => {
    const rel = relative(root, target);
    if (isAbsolute(rel) || rel === ".." || rel.startsWith(`..${sep}`))
      throw new Error("Skill path escapes configured root.");
  };
  return {
    id,
    async list(signal) {
      signal?.throwIfAborted();
      const root = await realpath(configuredRoot);
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
          await realpath(resolve(root, entry.name, "SKILL.md"));
          result.push(entry.name);
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
        }
      }
      return result.sort();
    },
    async read(skill, path, signal) {
      signal?.throwIfAborted();
      const root = await realpath(configuredRoot);
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
      const handle = await open(
        target,
        constants.O_RDONLY | constants.O_NOFOLLOW,
      );
      try {
        const stat = await handle.stat();
        if (!stat.isFile() || stat.size > 1_000_000)
          throw new Error("Skill resource is not a bounded regular file.");
        signal?.throwIfAborted();
        const bytes = Buffer.alloc(1_000_001);
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
          if (total > 1_000_000)
            throw new Error("Skill resource exceeds byte limit.");
        }
        return bytes.subarray(0, total).toString("utf8");
      } finally {
        await handle.close();
      }
    },
  };
}
