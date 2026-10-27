import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { join } from "node:path";
import type { ProjectInstructionsSource } from "@agentkit/host";

/**
 * Reads one instruction file (`AGENTS.md`, `CLAUDE.md`) from a workspace root.
 *
 * Confinement mirrors {@link fileSkillSource}: the root is resolved with
 * `realpath` (a symlinked ROOT is the caller's explicit choice), but the FILE
 * must be a regular file directly under it and never a symlink -- a repository
 * can ship a link named `AGENTS.md` pointing at `~/.ssh/config`, and its text
 * would reach the model. `O_NOFOLLOW` plus the lstat/fstat identity check
 * closes the swap between the check and the open.
 *
 * A missing file is `undefined`. A refusal (symlink, escape, special file) is a
 * throw, so the caller can tell "no instructions" from "instructions withheld".
 * Oversize files are truncated to `maxBytes`, not refused: a long AGENTS.md
 * still carries useful rules in its first 64 KiB.
 */
export function fileProjectInstructions(): ProjectInstructionsSource {
  return {
    async read(root, name, maxBytes) {
      if (
        name === "" ||
        name === "." ||
        name === ".." ||
        name.includes("/") ||
        name.includes("\\") ||
        name.includes("\0")
      )
        throw new Error("Instruction file name must be a plain file name.");
      const realRoot = await realpath(root);
      const target = join(realRoot, name);
      let expected: Awaited<ReturnType<typeof lstat>>;
      try {
        expected = await lstat(target);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT")
          return undefined;
        throw error;
      }
      if (expected.isSymbolicLink())
        throw new Error("Instruction file is a symlink.");
      if (!expected.isFile())
        throw new Error("Instruction file is not a regular file.");
      const handle = await open(
        target,
        constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
      );
      try {
        const opened = await handle.stat();
        if (
          !opened.isFile() ||
          opened.dev !== expected.dev ||
          opened.ino !== expected.ino
        )
          throw new Error("Instruction file changed during open.");
        const bytes = Buffer.alloc(maxBytes + 1);
        let total = 0;
        while (total < bytes.length) {
          const { bytesRead } = await handle.read(
            bytes,
            total,
            bytes.length - total,
            null,
          );
          if (!bytesRead) break;
          total += bytesRead;
        }
        const truncated = total > maxBytes;
        let text = bytes
          .subarray(0, Math.min(total, maxBytes))
          .toString("utf8");
        // A cut inside a multi-byte character decodes to U+FFFD; drop it.
        if (truncated) text = text.replace(/�$/u, "");
        return { text, ...(truncated ? { truncated } : {}) };
      } finally {
        await handle.close();
      }
    },
  };
}
