import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

const sha = (s: string | Buffer) =>
  createHash("sha256").update(s).digest("hex");
const read = (root: string, rel: string) =>
  readFileSync(join(root, rel), "utf-8");

describe("edit_file", () => {
  test("single edit returns a unified diff and the new sha256", async () => {
    const h = await harness();
    put(h.root, "f.txt", "one\ntwo\nthree\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "two", newText: "TWO" }],
    });
    expect(r.ok).toBe(true);
    expect(read(h.root, "f.txt")).toBe("one\nTWO\nthree\n");
    const text = h.text(r);
    expect(text).toContain(`sha256: ${sha("one\nTWO\nthree\n")}`);
    expect(text).toContain("--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@");
    expect(text).toContain("\n one\n-two\n+TWO\n three");
    expect(text).toContain("+1 -1");
  });

  test("multiple disjoint edits match the ORIGINAL file", async () => {
    const h = await harness();
    put(h.root, "f.txt", "a\nb\nc\nd\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [
        { oldText: "a", newText: "b-new" },
        { oldText: "d", newText: "a" },
      ],
    });
    expect(r.ok).toBe(true);
    expect(read(h.root, "f.txt")).toBe("b-new\nb\nc\na\n");
  });

  test("no match is refused and the file is untouched", async () => {
    const h = await harness();
    put(h.root, "f.txt", "hello\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "absent", newText: "x" }],
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("edit_no_match");
    expect(read(h.root, "f.txt")).toBe("hello\n");
  });

  test("an ambiguous match is refused", async () => {
    const h = await harness();
    put(h.root, "f.txt", "x = 1\nx = 1\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "x = 1", newText: "x = 2" }],
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("edit_ambiguous");
    expect(read(h.root, "f.txt")).toBe("x = 1\nx = 1\n");
  });

  test("overlapping edits are refused", async () => {
    const h = await harness();
    put(h.root, "f.txt", "abcdef\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [
        { oldText: "abcd", newText: "1" },
        { oldText: "cdef", newText: "2" },
      ],
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("edit_overlap");
    expect(read(h.root, "f.txt")).toBe("abcdef\n");
  });

  test("an empty oldText and an unchanged result are refused", async () => {
    const h = await harness();
    put(h.root, "f.txt", "same\n");
    const empty = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "", newText: "x" }],
    });
    expect(h.text(empty)).toContain("edit_empty_old_text");
    const same = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "same", newText: "same" }],
    });
    expect(h.text(same)).toContain("edit_no_change");
  });

  test("CRLF line endings and a BOM are preserved", async () => {
    const h = await harness();
    put(h.root, "w.txt", "﻿one\r\ntwo\r\nthree\r\n");
    const r = await h.run("edit_file", {
      path: "w.txt",
      edits: [{ oldText: "two\nthree", newText: "2\n3\n4" }],
    });
    expect(r.ok).toBe(true);
    expect(read(h.root, "w.txt")).toBe("﻿one\r\n2\r\n3\r\n4\r\n");
  });

  test("falls back to normalized matching (trailing space, smart quotes)", async () => {
    const h = await harness();
    put(h.root, "q.txt", "keep me   \nconst a = “hi”;   \nkeep too\n");
    const r = await h.run("edit_file", {
      path: "q.txt",
      edits: [{ oldText: 'const a = "hi";', newText: 'const a = "bye";' }],
    });
    expect(r.ok).toBe(true);
    // untouched lines keep their original bytes, including trailing spaces
    expect(read(h.root, "q.txt")).toBe(
      'keep me   \nconst a = "bye";\nkeep too\n',
    );
  });

  test("expectedSha256 mismatch is refused; a match is accepted", async () => {
    const h = await harness();
    put(h.root, "f.txt", "v1\n");
    const bad = await h.run("edit_file", {
      path: "f.txt",
      expectedSha256: sha("something else"),
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    expect(bad.ok).toBe(false);
    expect(h.text(bad)).toContain("sha256_mismatch");
    expect(read(h.root, "f.txt")).toBe("v1\n");
    const good = await h.run("edit_file", {
      path: "f.txt",
      expectedSha256: sha("v1\n"),
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    expect(good.ok).toBe(true);
  });

  test("concurrent edits to one file are serialized, none lost", async () => {
    const h = await harness();
    const n = 25;
    put(
      h.root,
      "c.txt",
      Array.from({ length: n }, (_, i) => `T${i}=0`).join("\n") + "\n",
    );
    const results = await Promise.all(
      Array.from({ length: n }, (_, i) =>
        h.run("edit_file", {
          path: "c.txt",
          edits: [{ oldText: `T${i}=0`, newText: `T${i}=1` }],
        }),
      ),
    );
    expect(results.every((r) => r.ok)).toBe(true);
    expect(read(h.root, "c.txt")).toBe(
      Array.from({ length: n }, (_, i) => `T${i}=1`).join("\n") + "\n",
    );
    expect(readdirSync(h.root).filter((f) => f.endsWith(".tmp"))).toEqual([]);
  });

  test("a symlink alias queues with its target", async () => {
    const h = await harness();
    put(h.root, "real.txt", "a=0\nb=0\n");
    symlinkSync(join(h.root, "real.txt"), join(h.root, "alias.txt"));
    const [x, y] = await Promise.all([
      h.run("edit_file", {
        path: "real.txt",
        edits: [{ oldText: "a=0", newText: "a=1" }],
      }),
      h.run("edit_file", {
        path: "alias.txt",
        edits: [{ oldText: "b=0", newText: "b=1" }],
      }),
    ]);
    expect([x.ok, y.ok]).toEqual([true, true]);
    expect(read(h.root, "real.txt")).toBe("a=1\nb=1\n");
  });

  test("refuses a missing file, a binary file, .git and sensitive paths", async () => {
    const h = await harness();
    const edits = [{ oldText: "a", newText: "b" }];
    expect(
      h.text(await h.run("edit_file", { path: "nope.txt", edits })),
    ).toContain("not_found");
    put(h.root, "x.bin", "a\0b");
    expect(
      h.text(await h.run("edit_file", { path: "x.bin", edits })),
    ).toContain("binary_file");
    put(h.root, ".git/config", "a");
    expect((await h.run("edit_file", { path: ".git/config", edits })).ok).toBe(
      false,
    );
    put(h.root, ".env", "a");
    expect(h.text(await h.run("edit_file", { path: ".env", edits }))).toContain(
      "path_sensitive",
    );
    expect(read(h.root, ".env")).toBe("a");
  });

  test("rejects malformed edits and escapes outside the root", async () => {
    const h = await harness();
    put(h.root, "f.txt", "a");
    expect((await h.run("edit_file", { path: "f.txt", edits: [] })).ok).toBe(
      false,
    );
    expect(
      (await h.run("edit_file", { path: "f.txt", edits: [{ oldText: 1 }] })).ok,
    ).toBe(false);
    const outside = tempDir("outside-");
    put(outside, "o.txt", "a");
    const r = await h.run("edit_file", {
      path: join(outside, "o.txt"),
      edits: [{ oldText: "a", newText: "b" }],
    });
    expect(h.text(r)).toContain("path_outside_root");
    expect(read(outside, "o.txt")).toBe("a");
  });

  test("the diff of a far-apart double edit has two hunks", async () => {
    const h = await harness();
    const body =
      Array.from({ length: 40 }, (_, i) => `l${i}`).join("\n") + "\n";
    put(h.root, "d.txt", body);
    const r = await h.run("edit_file", {
      path: "d.txt",
      edits: [
        { oldText: "l2\n", newText: "L2\n" },
        { oldText: "l35\n", newText: "L35\n" },
      ],
    });
    const hunks = h
      .text(r)
      .split("\n")
      .filter((l) => l.startsWith("@@"));
    expect(hunks).toEqual(["@@ -1,6 +1,6 @@", "@@ -33,7 +33,7 @@"]);
  });
});

describe("write_file", () => {
  test("creates parents inside the root and reports a sha256", async () => {
    const h = await harness();
    const r = await h.run("write_file", { path: "a/b/c.txt", content: "hi\n" });
    expect(r.ok).toBe(true);
    expect(read(h.root, "a/b/c.txt")).toBe("hi\n");
    expect(h.text(r)).toContain(`sha256: ${sha("hi\n")}`);
    expect(h.text(r)).toContain("Created");
    const again = await h.run("write_file", {
      path: "a/b/c.txt",
      content: "yo\n",
    });
    expect(h.text(again)).toContain("Overwrote");
    expect(read(h.root, "a/b/c.txt")).toBe("yo\n");
  });

  test("refuses outside the root, .git, sensitive and oversized writes", async () => {
    const h = await harness({ limits: { writeMaxBytes: 10 } });
    const outside = tempDir("outside-");
    expect(
      h.text(
        await h.run("write_file", {
          path: join(outside, "x.txt"),
          content: "x",
        }),
      ),
    ).toContain("path_outside_root");
    expect(existsSync(join(outside, "x.txt"))).toBe(false);
    expect(
      h.text(await h.run("write_file", { path: "../esc.txt", content: "x" })),
    ).toContain("path_outside_root");
    expect(
      h.text(await h.run("write_file", { path: ".git/HEAD", content: "x" })),
    ).toContain("path_git_write");
    expect(
      h.text(await h.run("write_file", { path: ".env", content: "x" })),
    ).toContain("path_sensitive");
    expect(
      h.text(
        await h.run("write_file", { path: "big.txt", content: "x".repeat(11) }),
      ),
    ).toContain("content_too_large");
    expect((await h.run("write_file", { path: "n.txt", content: 5 })).ok).toBe(
      false,
    );
  });

  test("never writes through a symlink that leaves the root", async () => {
    const h = await harness();
    const outside = tempDir("outside-");
    symlinkSync(outside, join(h.root, "escape"));
    const r = await h.run("write_file", {
      path: "escape/new.txt",
      content: "x",
    });
    expect(h.text(r)).toContain("path_outside_root");
    expect(existsSync(join(outside, "new.txt"))).toBe(false);
  });

  test("writing over a directory fails cleanly", async () => {
    const h = await harness();
    put(h.root, "d/x.txt", "x");
    const r = await h.run("write_file", { path: "d", content: "x" });
    expect(r.ok).toBe(false);
  });
});
