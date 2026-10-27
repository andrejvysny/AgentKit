import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { confine } from "../src/paths.js";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

const strict = { access: "read", allowSensitive: false } as const;
const write = { access: "write", allowSensitive: false } as const;

describe("confinement", () => {
  test("refuses ../ and absolute escapes", async () => {
    const root = tempDir();
    await expect(confine(root, "../x", strict)).rejects.toMatchObject({
      code: "path_outside_root",
    });
    await expect(confine(root, "a/../../x", strict)).rejects.toMatchObject({
      code: "path_outside_root",
    });
    await expect(confine(root, "/etc/passwd", strict)).rejects.toMatchObject({
      code: "path_outside_root",
    });
  });

  test("allows an absolute path inside the root and the root itself", async () => {
    const root = tempDir();
    put(root, "a.txt", "x");
    expect(
      (await confine(root, join(root, "a.txt"), strict)).relativePath,
    ).toBe("a.txt");
    expect((await confine(root, ".", strict)).relativePath).toBe("");
  });

  test("refuses a symlink pointing outside, for read and write", async () => {
    const root = tempDir();
    const outside = tempDir("outside-");
    put(outside, "secret.txt", "s");
    symlinkSync(outside, join(root, "link"));
    symlinkSync(join(outside, "secret.txt"), join(root, "file-link"));
    for (const target of [
      "link",
      "link/secret.txt",
      "file-link",
      "link/new.txt",
    ]) {
      await expect(confine(root, target, strict)).rejects.toMatchObject({
        code: "path_outside_root",
      });
      await expect(confine(root, target, write)).rejects.toMatchObject({
        code: "path_outside_root",
      });
    }
  });

  test("refuses a dangling symlink", async () => {
    const root = tempDir();
    symlinkSync(join(root, "missing-target"), join(root, "dangling"));
    await expect(confine(root, "dangling", write)).rejects.toMatchObject({
      code: "path_dangling_symlink",
    });
    await expect(confine(root, "dangling", strict)).rejects.toMatchObject({
      code: "path_dangling_symlink",
    });
  });

  test("refuses ~ and malformed paths", async () => {
    const root = tempDir();
    for (const bad of ["~", "~/x", "", "a\0b"])
      await expect(confine(root, bad, strict)).rejects.toMatchObject({
        code: "path_invalid",
      });
    await expect(confine(root, 5, strict)).rejects.toMatchObject({
      code: "path_invalid",
    });
  });

  test("a symlink inside the root that points at a sensitive file is refused", async () => {
    const root = tempDir();
    put(root, ".env", "A=1");
    symlinkSync(join(root, ".env"), join(root, "innocent.txt"));
    await expect(confine(root, "innocent.txt", strict)).rejects.toMatchObject({
      code: "path_sensitive",
    });
  });

  test("a new file under a missing directory resolves inside the root", async () => {
    const root = tempDir();
    const got = await confine(root, "a/b/c.txt", write);
    expect(got.relativePath).toBe(join("a", "b", "c.txt"));
  });
});

const SENSITIVE = [
  ".ssh/config",
  ".aws/credentials",
  ".gnupg/key",
  ".kube/config",
  ".docker/config.json",
  ".azure/x",
  ".terraform.d/x",
  "server.pem",
  "a.key",
  "a.p12",
  "a.pfx",
  "a.jks",
  "a.keystore",
  "a.p8",
  "a.jwk",
  "a.kdbx",
  ".netrc",
  ".npmrc",
  ".pypirc",
  ".git-credentials",
  ".htpasswd",
  "id_rsa",
  "id_rsa.pub",
  "credentials.json",
  "secrets.yaml",
  "service-account.json",
  ".env",
  ".env.local",
  ".env.production",
  "nested/dir/.env",
  ".SSH/config",
];

describe("sensitive denylist", () => {
  test.each(SENSITIVE)("%s is refused for read and write", async (rel) => {
    const root = tempDir();
    await expect(confine(root, rel, strict)).rejects.toMatchObject({
      code: "path_sensitive",
    });
    await expect(confine(root, rel, write)).rejects.toMatchObject({
      code: "path_sensitive",
    });
  });

  test.each(SENSITIVE)("%s is allowed with allowSensitive", async (rel) => {
    const root = tempDir();
    await expect(
      confine(root, rel, { access: "read", allowSensitive: true }),
    ).resolves.toBeDefined();
    await expect(
      confine(root, rel, { access: "write", allowSensitive: true }),
    ).resolves.toBeDefined();
  });

  test.each([
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    "config.ts",
  ])("%s is a normal file", async (rel) => {
    const root = tempDir();
    await expect(confine(root, rel, strict)).resolves.toBeDefined();
  });

  test(".git: read refused by default, write refused even with allowSensitive", async () => {
    const root = tempDir();
    mkdirSync(join(root, ".git"));
    await expect(confine(root, ".git/config", strict)).rejects.toMatchObject({
      code: "path_sensitive",
    });
    await expect(confine(root, ".git/hooks/x", write)).rejects.toMatchObject({
      code: "path_git_write",
    });
    const open = { access: "write", allowSensitive: true } as const;
    await expect(confine(root, ".git/hooks/x", open)).rejects.toMatchObject({
      code: "path_git_write",
    });
    await expect(
      confine(root, ".git/HEAD", { access: "read", allowSensitive: true }),
    ).resolves.toBeDefined();
  });

  test("tools enforce it end to end", async () => {
    const h = await harness();
    put(h.root, ".env", "TOKEN=abc");
    const read = await h.run("read_file", { path: ".env" });
    expect(read.ok).toBe(false);
    expect(h.text(read)).toContain("path_sensitive");
    expect(h.text(read)).not.toContain("abc");
    const write2 = await h.run("write_file", {
      path: ".git/hooks/pre-commit",
      content: "x",
    });
    expect(write2.ok).toBe(false);
    const permissive = await harness({ allowSensitive: true });
    put(permissive.root, ".env", "TOKEN=abc");
    expect((await permissive.run("read_file", { path: ".env" })).ok).toBe(true);
  });
});
