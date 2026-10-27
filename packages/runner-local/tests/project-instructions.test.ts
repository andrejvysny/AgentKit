import { afterEach, beforeEach, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileProjectInstructions } from "../src/project-instructions.js";

let dir: string;
beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), "agentkit-instructions-"));
});
afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

const source = fileProjectInstructions();
const MAX = 64 * 1024;

test("reads AGENTS.md and CLAUDE.md independently", async () => {
  await writeFile(join(dir, "AGENTS.md"), "agents rules");
  await writeFile(join(dir, "CLAUDE.md"), "claude rules");
  expect(await source.read(dir, "AGENTS.md", MAX)).toEqual({
    text: "agents rules",
  });
  expect(await source.read(dir, "CLAUDE.md", MAX)).toEqual({
    text: "claude rules",
  });
});

test("falls back to CLAUDE.md when AGENTS.md is absent", async () => {
  await writeFile(join(dir, "CLAUDE.md"), "only claude");
  expect(await source.read(dir, "AGENTS.md", MAX)).toBeUndefined();
  expect((await source.read(dir, "CLAUDE.md", MAX))?.text).toBe("only claude");
});

test("truncates an oversize file at the cap and says so", async () => {
  await writeFile(join(dir, "AGENTS.md"), "a".repeat(MAX + 100));
  const result = await source.read(dir, "AGENTS.md", MAX);
  expect(result?.text.length).toBe(MAX);
  expect(result?.truncated).toBe(true);
});

test("refuses a symlinked file, even one pointing inside the root", async () => {
  await writeFile(join(dir, "real.md"), "real");
  await symlink(join(dir, "real.md"), join(dir, "AGENTS.md"));
  await expect(source.read(dir, "AGENTS.md", MAX)).rejects.toThrow("symlink");
});

test("refuses a symlink that escapes the root", async () => {
  const outside = await mkdtemp(join(tmpdir(), "agentkit-outside-"));
  try {
    await writeFile(join(outside, "secret"), "secret");
    await symlink(join(outside, "secret"), join(dir, "CLAUDE.md"));
    await expect(source.read(dir, "CLAUDE.md", MAX)).rejects.toThrow();
  } finally {
    await rm(outside, { recursive: true, force: true });
  }
});

test("resolves a symlinked root with realpath", async () => {
  await mkdir(join(dir, "real"));
  await writeFile(join(dir, "real/AGENTS.md"), "via link");
  await symlink(join(dir, "real"), join(dir, "link"));
  expect((await source.read(join(dir, "link"), "AGENTS.md", MAX))?.text).toBe(
    "via link",
  );
});

test("answers undefined for a missing file", async () => {
  expect(await source.read(dir, "AGENTS.md", MAX)).toBeUndefined();
});

test("refuses names that leave the root", async () => {
  await writeFile(join(dir, "AGENTS.md"), "x");
  for (const name of ["../AGENTS.md", "sub/AGENTS.md", "..", "", "/etc/passwd"])
    await expect(source.read(dir, name, MAX)).rejects.toThrow(
      "plain file name",
    );
});

test("refuses a directory named like an instruction file", async () => {
  await mkdir(join(dir, "AGENTS.md"));
  await expect(source.read(dir, "AGENTS.md", MAX)).rejects.toThrow(
    "regular file",
  );
});
