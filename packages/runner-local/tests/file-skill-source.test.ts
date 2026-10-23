import { expect, test } from "bun:test";
import {
  mkdtemp,
  mkdir,
  writeFile,
  symlink,
  unlink,
  rm,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileSkillSource } from "../src/file-skill-source.js";
test("configured filesystem skills refuse traversal, symlink escape and oversized resources", async () => {
  const root = await mkdtemp(join(tmpdir(), "agentkit-skills-"));
  try {
    await mkdir(join(root, "skills/review"), { recursive: true });
    await writeFile(
      join(root, "skills/review/SKILL.md"),
      "---\nname: review\ndescription: test\n---\ninstructions",
    );
    await writeFile(join(root, "outside"), "private");
    await symlink(join(root, "outside"), join(root, "skills/review/escape"));
    await writeFile(join(root, "skills/review/large"), "a".repeat(1_000_001));
    const source = fileSkillSource("project", join(root, "skills"));
    expect(await source.list()).toEqual(["review"]);
    expect(await source.read("review", "SKILL.md")).toContain("instructions");
    await expect(source.read("review", "../outside")).rejects.toThrow();
    await expect(source.read("review", "escape")).rejects.toThrow(
      "escapes configured root",
    );
    await expect(source.read("review", "large")).rejects.toThrow(
      "bounded regular file",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("filesystem source keeps the initially selected root when its configured symlink changes", async () => {
  const root = await mkdtemp(join(tmpdir(), "agentkit-skills-root-"));
  try {
    await mkdir(join(root, "trusted"));
    await mkdir(join(root, "other"));
    await writeFile(join(root, "trusted/SKILL.md"), "trusted instructions");
    await writeFile(join(root, "other/SKILL.md"), "other instructions");
    await symlink(join(root, "trusted"), join(root, "selected"));
    const source = fileSkillSource("project", join(root, "selected"));
    expect(await source.list()).toEqual(["."]);
    await unlink(join(root, "selected"));
    await symlink(join(root, "other"), join(root, "selected"));
    expect(await source.read(".", "SKILL.md")).toBe("trusted instructions");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("filesystem source rejects special files and checks cancellation", async () => {
  const root = await mkdtemp(join(tmpdir(), "agentkit-skills-kind-"));
  try {
    await writeFile(join(root, "SKILL.md"), "instructions");
    await mkdir(join(root, "directory"));
    const source = fileSkillSource("project", root);
    await expect(source.read(".", "directory")).rejects.toThrow(
      "bounded regular file",
    );
    const controller = new AbortController();
    controller.abort(new Error("cancelled"));
    await expect(
      source.read(".", "SKILL.md", controller.signal),
    ).rejects.toThrow("cancelled");
    await expect(source.list(controller.signal)).rejects.toThrow("cancelled");
    await expect(source.read("..", "SKILL.md")).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("parent-directory replacement between resolution and open cannot disclose an outside file", async () => {
  const root = await mkdtemp(join(tmpdir(), "agentkit-skills-race-"));
  try {
    await mkdir(join(root, "skills/review"), { recursive: true });
    await mkdir(join(root, "outside"));
    await writeFile(
      join(root, "skills/review/SKILL.md"),
      "trusted instructions",
    );
    await writeFile(join(root, "outside/SKILL.md"), "outside secret");
    const sourcePath = new URL("../src/file-skill-source.ts", import.meta.url)
      .href;
    const script = `
      import { mock } from "bun:test";
      const filesystem = await import("node:fs/promises");
      const originalOpen = filesystem.open;
      const root = ${JSON.stringify(root)};
      let replaced = false;
      mock.module("node:fs/promises", () => ({
        ...filesystem,
        open: async (...args) => {
          if (!replaced) {
            replaced = true;
            await filesystem.rename(root + "/skills/review", root + "/skills/saved");
            await filesystem.symlink(root + "/outside", root + "/skills/review");
          }
          return originalOpen(...args);
        },
      }));
      const { fileSkillSource } = await import(${JSON.stringify(sourcePath)});
      try {
        await fileSkillSource("project", root + "/skills").read("review", "SKILL.md");
        process.exit(1);
      } catch (error) {
        if (!replaced || !String(error).includes("changed during open")) throw error;
      }
    `;
    const child = Bun.spawn([process.execPath, "--eval", script], {
      stdout: "pipe",
      stderr: "pipe",
    });
    const [exitCode, stderr] = await Promise.all([
      child.exited,
      new Response(child.stderr).text(),
    ]);
    expect(stderr).toBe("");
    expect(exitCode).toBe(0);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
