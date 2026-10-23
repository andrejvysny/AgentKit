import { expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, symlink, rm } from "node:fs/promises";
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
