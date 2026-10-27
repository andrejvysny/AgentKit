import { afterEach, describe, expect, test } from "bun:test";
import { symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { cleanup, harness, put } from "./support.js";

afterEach(cleanup);

const lines = (n: number, make = (i: number) => `line ${i}`) =>
  Array.from({ length: n }, (_, i) => make(i + 1)).join("\n");

describe("read_file", () => {
  test("numbers lines with a tab and no notice for a short file", async () => {
    const h = await harness();
    put(h.root, "a.txt", "alpha\nbeta\ngamma\n");
    const r = await h.run("read_file", { path: "a.txt" });
    expect(r.ok).toBe(true);
    expect(h.text(r)).toBe("1\talpha\n2\tbeta\n3\tgamma");
    expect(r.truncated).toBe(false);
  });

  test("offset is 1-based and limit bounds the slice with a continuation notice", async () => {
    const h = await harness();
    put(h.root, "a.txt", lines(10));
    const r = await h.run("read_file", { path: "a.txt", offset: 3, limit: 2 });
    expect(h.text(r)).toBe(
      "3\tline 3\n4\tline 4\n[Showing lines 3-4 of 10. Use offset=5 to continue.]",
    );
    const tail = await h.run("read_file", { path: "a.txt", offset: 9 });
    expect(h.text(tail)).toBe("9\tline 9\n10\tline 10");
  });

  test("an offset past the end is a clear failure", async () => {
    const h = await harness();
    put(h.root, "a.txt", lines(3));
    const r = await h.run("read_file", { path: "a.txt", offset: 4 });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("offset_out_of_range");
  });

  test("stops at 2000 lines with the continuation notice", async () => {
    const h = await harness();
    put(
      h.root,
      "big.txt",
      lines(2500, () => "x"),
    );
    const r = await h.run("read_file", { path: "big.txt" });
    expect(r.ok).toBe(true);
    expect(r.truncated).toBe(true);
    const text = h.text(r);
    expect(text).toContain("2000\tx");
    expect(text).not.toContain("2001\tx");
    expect(text).toEndWith(
      "[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]",
    );
    const next = await h.run("read_file", { path: "big.txt", offset: 2001 });
    expect(h.text(next)).toContain("2500\tx");
    expect(next.truncated).toBe(false);
  });

  test("cuts at 50 KB on a line boundary", async () => {
    const h = await harness();
    put(
      h.root,
      "wide.txt",
      lines(100, () => "y".repeat(1000)),
    );
    const r = await h.run("read_file", { path: "wide.txt" });
    const text = h.text(r);
    expect(r.truncated).toBe(true);
    expect(Buffer.byteLength(text)).toBeLessThan(51_200 + 200);
    expect(text).toContain("50.0KB limit");
    expect(text).toMatch(/Use offset=\d+ to continue\.\]$/);
  });

  test("a single line over the byte limit is refused clearly", async () => {
    const h = await harness();
    put(h.root, "one.txt", "z".repeat(60_000));
    const r = await h.run("read_file", { path: "one.txt" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("line_too_long");
  });

  test("a binary file is refused", async () => {
    const h = await harness();
    writeFileSync(join(h.root, "b.bin"), Buffer.from([1, 2, 0, 3, 4]));
    const r = await h.run("read_file", { path: "b.bin" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("binary_file");
  });

  test("a missing file, a directory and an oversized file", async () => {
    const h = await harness({ limits: { readMaxFileBytes: 100 } });
    const missing = await h.run("read_file", { path: "nope.txt" });
    expect(missing.ok).toBe(false);
    expect(h.text(missing)).toContain("not_found");
    put(h.root, "dir/x.txt", "x");
    expect(h.text(await h.run("read_file", { path: "dir" }))).toContain(
      "not_a_file",
    );
    put(h.root, "huge.txt", "q".repeat(101));
    expect(h.text(await h.run("read_file", { path: "huge.txt" }))).toContain(
      "file_too_large",
    );
  });

  test("an empty file and bad arguments", async () => {
    const h = await harness();
    put(h.root, "e.txt", "");
    expect(h.text(await h.run("read_file", { path: "e.txt" }))).toBe(
      "(empty file)",
    );
    const bad = await h.run("read_file", { path: "e.txt", offset: 0 });
    expect(bad.ok).toBe(false);
    expect(h.text(bad)).toContain("invalid_arguments");
    expect((await h.run("read_file", null)).ok).toBe(false);
  });

  test("a symlink to a file inside the root reads the real file", async () => {
    const h = await harness();
    put(h.root, "real.txt", "hello");
    symlinkSync(join(h.root, "real.txt"), join(h.root, "alias.txt"));
    expect(h.text(await h.run("read_file", { path: "alias.txt" }))).toBe(
      "1\thello",
    );
  });
});

describe("list_dir", () => {
  test("marks directories, hides credentials, caps entries", async () => {
    const h = await harness({ limits: { listMaxEntries: 3 } });
    put(h.root, "a/x.txt", "x");
    put(h.root, "b.txt", "b");
    put(h.root, "c.txt", "c");
    put(h.root, "d.txt", "d");
    put(h.root, ".env", "A=1");
    const r = await h.run("list_dir", {});
    expect(h.text(r)).toBe(
      "a/\nb.txt\nc.txt\n[1 more entries not shown]\n[1 sensitive entries hidden]",
    );
    expect(r.truncated).toBe(true);
    const sub = await h.run("list_dir", { path: "a" });
    expect(h.text(sub)).toBe("x.txt");
  });
});
