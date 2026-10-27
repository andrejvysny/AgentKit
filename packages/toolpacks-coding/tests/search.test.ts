import { afterEach, describe, expect, test } from "bun:test";
import { symlinkSync } from "node:fs";
import { join } from "node:path";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

describe("code_search", () => {
  test("finds matches with path:line:text and honours .gitignore", async () => {
    const h = await harness();
    put(h.root, ".gitignore", "ignored/\n*.log\n!keep.log\n");
    put(h.root, "src/a.ts", "const needle = 1;\nother\n");
    put(h.root, "ignored/b.ts", "needle in ignored dir\n");
    put(h.root, "x.log", "needle in log\n");
    put(h.root, "keep.log", "needle in kept log\n");
    const r = await h.run("code_search", { pattern: "needle" });
    expect(r.ok).toBe(true);
    const text = h.text(r);
    expect(text).toContain("src/a.ts:1:const needle = 1;");
    expect(text).toContain("keep.log:1:needle in kept log");
    expect(text).not.toContain("ignored/b.ts");
    expect(text).not.toContain("x.log");
  });

  test("honours .ignore and skips .git, node_modules, symlinks, binary", async () => {
    const h = await harness();
    put(h.root, ".ignore", "scratch.txt\n");
    put(h.root, "scratch.txt", "needle\n");
    put(h.root, ".git/HEAD", "needle\n");
    put(h.root, "node_modules/p/i.js", "needle\n");
    put(h.root, "bin.dat", "needle\0\n");
    const outside = tempDir("outside-");
    put(outside, "o.txt", "needle\n");
    symlinkSync(join(outside, "o.txt"), join(h.root, "link.txt"));
    symlinkSync(outside, join(h.root, "linkdir"));
    put(h.root, "real.txt", "needle\n");
    const text = h.text(await h.run("code_search", { pattern: "needle" }));
    expect(text).toBe("real.txt:1:needle");
  });

  test("sensitive files are skipped, even when searched directly", async () => {
    const h = await harness();
    put(h.root, ".env", "API=needle\n");
    put(h.root, "id_rsa", "needle\n");
    put(h.root, "deploy.pem", "needle\n");
    put(h.root, ".env.example", "API=needle-example\n");
    const text = h.text(await h.run("code_search", { pattern: "needle" }));
    expect(text).toBe(".env.example:1:API=needle-example");
    const direct = await h.run("code_search", {
      pattern: "needle",
      path: ".env",
    });
    expect(direct.ok).toBe(false);
    expect(h.text(direct)).toContain("path_sensitive");
  });

  test("literal, ignoreCase, glob, context and per-line cut", async () => {
    const h = await harness();
    put(h.root, "a.ts", "l1\nl2\nHello (World)\nl4\nl5\n");
    put(h.root, "b.md", "hello (world)\n");
    const lit = h.text(
      await h.run("code_search", {
        pattern: "hello (world)",
        literal: true,
        ignoreCase: true,
        glob: "*.ts",
        context: 1,
      }),
    );
    expect(lit).toBe("a.ts:2:l2\na.ts:3:Hello (World)\na.ts:4:l4");
    const bad = await h.run("code_search", { pattern: "(", path: "a.ts" });
    expect(bad.ok).toBe(false);
    expect(h.text(bad)).toContain("invalid_pattern");
    put(h.root, "long.txt", `x${"y".repeat(3000)}\n`);
    const cut = h.text(
      await h.run("code_search", { pattern: "^x", path: "long.txt" }),
    );
    expect(cut.length).toBe("long.txt:1:".length + 1024);
  });

  test("match limit and stop notices", async () => {
    const h = await harness();
    put(h.root, "m.txt", Array.from({ length: 10 }, () => "hit").join("\n"));
    const r = await h.run("code_search", { pattern: "hit", limit: 3 });
    expect(r.truncated).toBe(true);
    expect(h.text(r)).toContain(
      "[stopped: match limit 3 reached; 3 matches so far",
    );
    expect(
      (await h.run("code_search", { pattern: "hit", limit: 5000 })).ok,
    ).toBe(false);
    expect(
      (await h.run("code_search", { pattern: "hit", context: 21 })).ok,
    ).toBe(false);

    const small = await harness({ limits: { searchMaxFiles: 5 } });
    for (let i = 0; i < 20; i++)
      put(small.root, `f${String(i).padStart(2, "0")}.txt`, "zzz\n");
    const stopped = await small.run("code_search", { pattern: "nomatch" });
    expect(small.text(stopped)).toContain(
      "No matches found (5 entries scanned;",
    );
    expect(stopped.truncated).toBe(true);
  });

  test("a time budget stops the walk and says so", async () => {
    let now = 0;
    const h = await harness({
      limits: { searchTimeBudgetMs: 10 },
      clock: () => (now += 6),
    });
    for (let i = 0; i < 10; i++) put(h.root, `d${i}/f.txt`, "zzz\n");
    const r = await h.run("code_search", { pattern: "zzz" });
    expect(h.text(r)).toContain("s of scanning elapsed");
    expect(r.truncated).toBe(true);
  });

  test("an aborted search fails with aborted", async () => {
    const h = await harness();
    put(h.root, "a.txt", "x");
    const ac = new AbortController();
    ac.abort();
    const r = await h.run("code_search", { pattern: "x" }, ac.signal);
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("aborted");
  });

  test("refuses a search path outside the root", async () => {
    const h = await harness();
    const r = await h.run("code_search", { pattern: "x", path: ".." });
    expect(h.text(r)).toContain("path_outside_root");
  });
});

describe("find_files", () => {
  test("globs, braces, gitignore, sensitive skip and the limit", async () => {
    const h = await harness();
    put(h.root, ".gitignore", "dist/\n");
    put(h.root, "src/a.ts", "");
    put(h.root, "src/deep/b.tsx", "");
    put(h.root, "src/c.md", "");
    put(h.root, "dist/d.ts", "");
    put(h.root, ".env", "");
    put(h.root, "keys.pem", "");
    expect(h.text(await h.run("find_files", { pattern: "**/*.ts" }))).toBe(
      "src/a.ts",
    );
    expect(
      h.text(await h.run("find_files", { pattern: "src/**/*.{ts,tsx}" })),
    ).toBe("src/a.ts\nsrc/deep/b.tsx");
    expect(
      h.text(await h.run("find_files", { pattern: "*.md", path: "src" })),
    ).toBe("src/c.md");
    expect(h.text(await h.run("find_files", { pattern: "**/.env*" }))).toBe(
      "No files matched",
    );
    expect(h.text(await h.run("find_files", { pattern: "*.pem" }))).toBe(
      "No files matched",
    );
    const one = await h.run("find_files", { pattern: "**/*", limit: 1 });
    expect(h.text(one)).toContain("[stopped: result limit 1 reached]");
    expect(one.truncated).toBe(true);
  });

  test("a bad glob and a non-directory path fail clearly", async () => {
    const h = await harness();
    put(h.root, "f.txt", "");
    expect(
      h.text(await h.run("find_files", { pattern: "*", path: "f.txt" })),
    ).toContain("not_a_directory");
    expect((await h.run("find_files", { pattern: "x".repeat(5000) })).ok).toBe(
      false,
    );
  });
});

describe("repo_map", () => {
  test("shows manifests and a depth-limited tree", async () => {
    const h = await harness();
    put(h.root, "package.json", '{"name":"demo"}');
    put(h.root, "README.md", "# Demo\n");
    put(h.root, "src/a/b/c/d.ts", "");
    put(h.root, "src/index.ts", "");
    put(h.root, ".gitignore", "build/\n");
    put(h.root, "build/out.js", "");
    put(h.root, ".env", "A=1");
    const r = await h.run("repo_map", {});
    const text = h.text(r);
    expect(text).toContain('== package.json ==\n{"name":"demo"}');
    expect(text).toContain("== README.md ==\n# Demo");
    expect(text).toContain("== tree (depth 3) ==");
    expect(text).toContain("src/\n  a/\n    b/\n  index.ts");
    expect(text).not.toContain("\n      c/");
    expect(text).not.toContain("build");
    expect(text).not.toContain(".env");
    const deep = h.text(await h.run("repo_map", { depth: 5 }));
    expect(deep).toContain("d.ts");
  });

  test("caps tree entries and says so", async () => {
    const h = await harness({ limits: { repoMapMaxEntries: 4 } });
    for (let i = 0; i < 10; i++) put(h.root, `f${i}.txt`, "");
    const r = await h.run("repo_map", {});
    expect(r.truncated).toBe(true);
    expect(h.text(r)).toContain("4 entries shown");
  });
});
