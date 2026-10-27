import { afterEach, describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { buildGitArgs, GIT_SAFETY_ARGS } from "../src/git-tool.js";
import { cleanup, harness, put } from "./support.js";

afterEach(cleanup);

function git(root: string, ...args: string[]): string {
  return execFileSync(
    "git",
    [
      "-C",
      root,
      "-c",
      "user.name=T",
      "-c",
      "user.email=t@example.com",
      "-c",
      "commit.gpgsign=false",
      ...args,
    ],
    { encoding: "utf8" },
  );
}

async function repo() {
  const h = await harness();
  git(h.root, "init", "-q", "-b", "main");
  put(h.root, "a.txt", "one\ntwo\n");
  put(h.root, ".env", "SECRET=1\n");
  git(h.root, "add", "-f", "a.txt", ".env");
  git(h.root, "commit", "-q", "-m", "first commit");
  return h;
}

describe("git_inspect", () => {
  test("status, diff, log and show in a temp repo", async () => {
    const h = await repo();
    put(h.root, "a.txt", "one\nTWO\n");
    put(h.root, "new.txt", "n\n");
    const status = h.text(await h.run("git_inspect", { command: "status" }));
    expect(status).toContain("## main");
    expect(status).toContain(" M a.txt");
    expect(status).toContain("?? new.txt");
    const diff = h.text(await h.run("git_inspect", { command: "diff" }));
    expect(diff).toContain("-two");
    expect(diff).toContain("+TWO");
    git(h.root, "add", "a.txt");
    expect(h.text(await h.run("git_inspect", { command: "diff" }))).toBe(
      "(no output)",
    );
    const staged = h.text(
      await h.run("git_inspect", { command: "diff", args: { staged: true } }),
    );
    expect(staged).toContain("+TWO");
    const log = h.text(
      await h.run("git_inspect", { command: "log", args: { maxCount: 5 } }),
    );
    expect(log).toMatch(/^[0-9a-f]+ \d{4}-\d\d-\d\d T first commit\n$/);
    const show = h.text(
      await h.run("git_inspect", { command: "show", args: { rev: "HEAD" } }),
    );
    expect(show).toContain("first commit");
    expect(show).toContain("+one");
  });

  test("a path argument narrows the output and is confined", async () => {
    const h = await repo();
    put(h.root, "a.txt", "changed\n");
    put(h.root, "b.txt", "x\n");
    git(h.root, "add", "b.txt");
    const status = h.text(
      await h.run("git_inspect", {
        command: "status",
        args: { path: "b.txt" },
      }),
    );
    expect(status).toContain("A  b.txt");
    expect(status).not.toContain("a.txt");
    expect(
      h.text(
        await h.run("git_inspect", { command: "diff", args: { path: "../x" } }),
      ),
    ).toContain("path_outside_root");
    expect(
      h.text(
        await h.run("git_inspect", {
          command: "show",
          args: { path: ".git/config" },
        }),
      ),
    ).toContain("path_sensitive");
  });

  test("changes to sensitive tracked files never appear in diff/show/status", async () => {
    const h = await repo();
    put(h.root, ".env", "SECRET=changed-secret\n");
    put(h.root, "a.txt", "one\nTWO\n");
    const diff = h.text(await h.run("git_inspect", { command: "diff" }));
    expect(diff).toContain("+TWO");
    expect(diff).not.toContain("changed-secret");
    const status = h.text(await h.run("git_inspect", { command: "status" }));
    expect(status).not.toContain(".env");
    expect(
      h.text(await h.run("git_inspect", { command: "show" })),
    ).not.toContain("SECRET=1");
  });

  test("a bad rev is rejected, including option-shaped ones", async () => {
    const h = await repo();
    for (const rev of [
      "--output=/tmp/x",
      "-p",
      "a b",
      "x;rm",
      "HEAD:.env",
      "a".repeat(101),
      "$(id)",
      "`id`",
    ]) {
      const r = await h.run("git_inspect", { command: "show", args: { rev } });
      expect(r.ok).toBe(false);
      expect(h.text(r)).toMatch(/invalid_rev|invalid_arguments/);
    }
    expect(
      (await h.run("git_inspect", { command: "log", args: { rev: "HEAD~1" } }))
        .ok,
    ).toBe(false); // valid shape, no such commit
  });

  test("any other command or argument shape is rejected", async () => {
    const h = await repo();
    for (const command of [
      "push",
      "checkout",
      "config",
      "reset",
      "log --all",
      "status;id",
      "-h",
    ]) {
      const r = await h.run("git_inspect", { command });
      expect(r.ok).toBe(false);
      expect(h.text(r)).toContain("git_command_not_allowed");
    }
    expect(
      (await h.run("git_inspect", { command: "log", args: { maxCount: 51 } }))
        .ok,
    ).toBe(false);
    expect(
      (await h.run("git_inspect", { command: "log", args: { extra: 1 } })).ok,
    ).toBe(true); // schema layer rejects extras; body ignores them
    expect((await h.run("git_inspect", {})).ok).toBe(false);
  });

  test("hooks and fsmonitor from the repository never run", async () => {
    const h = await repo();
    const marker = join(h.root, "..", `marker-${Date.now()}`);
    const script = join(h.root, "..", `fsmon-${Date.now()}.sh`);
    writeFileSync(script, `#!/bin/sh\ntouch '${marker}'\nprintf '\\0'\n`);
    chmodSync(script, 0o755);
    git(h.root, "config", "core.fsmonitor", script);
    // Baseline: a plain git status does run the configured fsmonitor hook.
    git(h.root, "status", "--porcelain");
    expect(existsSync(marker)).toBe(true);
    execFileSync("rm", [marker]);
    const r = await h.run("git_inspect", { command: "status" });
    expect(r.ok).toBe(true);
    expect(existsSync(marker)).toBe(false);
    execFileSync("rm", ["-f", script]);
  });

  describe("repository-configured drivers never run", () => {
    // git skips a clean filter when the index stat data is fresh, so the
    // baseline touches the file to force the filter, as an attacker would.
    async function trapped(setup: (root: string, marker: string) => void) {
      const h = await repo();
      const marker = join(h.root, "..", `drv-${Date.now()}-${Math.random()}`);
      put(h.root, ".gitattributes", "* filter=x\n*.txt diff=y\n");
      setup(h.root, marker);
      put(h.root, "a.txt", "one\ntwo\nthree\n");
      return { h, marker };
    }
    const touch = (m: string) => `touch '${m}'; cat`;

    const cases: Array<[string, (root: string, marker: string) => void]> = [
      [
        "clean filter",
        (root, m) => git(root, "config", "filter.x.clean", touch(m)),
      ],
      [
        "smudge filter",
        (root, m) => {
          git(root, "config", "filter.x.smudge", touch(m));
          git(root, "config", "filter.x.clean", "cat");
        },
      ],
      [
        "process filter",
        (root, m) => git(root, "config", "filter.x.process", `touch '${m}'`),
      ],
      [
        "textconv",
        (root, m) => git(root, "config", "diff.y.textconv", touch(m)),
      ],
      [
        "diff.external",
        (root, m) => git(root, "config", "diff.external", `touch '${m}'; :`),
      ],
      [
        "include.path",
        (root, m) => {
          put(root, "../inc.cfg", `[filter "x"]\n\tclean = ${touch(m)}\n`);
          git(root, "config", "include.path", join(root, "..", "inc.cfg"));
        },
      ],
    ];

    for (const [name, setup] of cases) {
      test(`${name} is neutralized for status, diff, show`, async () => {
        const { h, marker } = await trapped(setup);
        for (const command of ["status", "diff", "show"]) {
          const r = await h.run("git_inspect", { command });
          expect(r.ok).toBe(true);
          expect(h.text(r).length).toBeGreaterThan(0);
        }
        expect(existsSync(marker)).toBe(false);
      });
    }

    test("an unreadable config fails closed", async () => {
      const h = await repo();
      put(h.root, ".git/config", "[core\n\tbroken = \n");
      const r = await h.run("git_inspect", { command: "status" });
      expect(r.ok).toBe(false);
      expect(h.text(r)).toContain("git_config_unreadable");
    });
  });

  test("the argv is fixed, hook-free and shell-free", () => {
    const args = buildGitArgs(
      "/r",
      { command: "diff", staged: true, rev: "HEAD~1", path: "src" },
      true,
    );
    expect(args.slice(0, 2)).toEqual(["-C", "/r"]);
    expect(args.slice(2, 2 + GIT_SAFETY_ARGS.length)).toEqual([
      ...GIT_SAFETY_ARGS,
    ]);
    expect(GIT_SAFETY_ARGS).toContain("core.hooksPath=/dev/null");
    expect(GIT_SAFETY_ARGS).toContain("core.fsmonitor=false");
    expect(GIT_SAFETY_ARGS).toContain("core.sshCommand=false");
    expect(args).toContain("--no-ext-diff");
    expect(args).toContain("--no-textconv");
    expect(args.indexOf("--")).toBeGreaterThan(args.indexOf("HEAD~1"));
    expect(args).toContain("src");
  });

  test("output is capped and says so", async () => {
    const h = await harness({ limits: { gitMaxOutputBytes: 300 } });
    git(h.root, "init", "-q", "-b", "main");
    put(h.root, "a.txt", "x\n");
    git(h.root, "add", "a.txt");
    git(h.root, "commit", "-q", "-m", "c");
    put(
      h.root,
      "a.txt",
      Array.from({ length: 200 }, (_, i) => `line ${i}`).join("\n"),
    );
    const r = await h.run("git_inspect", { command: "diff" });
    expect(r.truncated).toBe(true);
    expect(h.text(r)).toContain("[output truncated at 300B");
  });

  test("a directory that is not a repository fails with a message", async () => {
    const h = await harness();
    const r = await h.run("git_inspect", { command: "status" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("git_failed");
  });
});
