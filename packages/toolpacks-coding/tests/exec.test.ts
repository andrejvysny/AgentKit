import { afterEach, describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import { sanitizedEnv } from "../src/exec-tools.js";
import { killProcessTree } from "../src/process.js";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

const darwin = process.platform === "darwin";
const REASON = "needs macOS sandbox-exec";

function alive(pattern: string): boolean {
  try {
    execFileSync("pgrep", ["-f", pattern]);
    return true;
  } catch {
    return false;
  }
}

describe("run_command (sandbox off, portable)", () => {
  test("runs, reports the exit code, and warns that the sandbox is off", async () => {
    const h = await harness();
    const ok = await h.run("run_command", {
      command: "echo hello; echo oops >&2",
    });
    expect(ok.ok).toBe(true);
    expect(h.text(ok)).toBe("hello\noops\n[exit code 0]");
    expect(ok.warnings).toEqual(["sandbox is off: the command ran unconfined"]);
    const bad = await h.run("run_command", { command: "exit 3" });
    expect(bad.ok).toBe(false);
    expect(h.text(bad)).toBe("[exit code 3]");
    expect(bad.summary).toBe("exit code 3");
  });

  test("cwd is confined and must be a directory", async () => {
    const h = await harness();
    mkdirSync(join(h.root, "sub"));
    expect(
      h.text(
        await h.run("run_command", { command: 'basename "$PWD"', cwd: "sub" }),
      ),
    ).toContain("sub");
    expect(
      h.text(await h.run("run_command", { command: "pwd", cwd: ".." })),
    ).toContain("path_outside_root");
    put(h.root, "f.txt", "");
    expect(
      h.text(await h.run("run_command", { command: "pwd", cwd: "f.txt" })),
    ).toContain("not_a_directory");
  });

  test("bad arguments fail without throwing", async () => {
    const h = await harness();
    expect((await h.run("run_command", { command: "" })).ok).toBe(false);
    expect(
      (await h.run("run_command", { command: "x", timeoutSec: 0 })).ok,
    ).toBe(false);
    expect(
      (await h.run("run_command", { command: "x", timeoutSec: 601 })).ok,
    ).toBe(false);
  });

  test("timeout kills the whole process group", async () => {
    const h = await harness();
    const r = await h.run("run_command", {
      command: "sleep 7001 & sleep 7002 & wait",
      timeoutSec: 1,
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("[timed out after 1s; process group killed");
    expect(r.summary).toBe("timed out after 1s");
    expect(alive("sleep 700[12]")).toBe(false);
  }, 20_000);

  test("abort kills the group and reports it", async () => {
    const h = await harness();
    const ac = new AbortController();
    setTimeout(() => ac.abort(), 300);
    const r = await h.run(
      "run_command",
      { command: "sleep 7003 & sleep 7004 & wait" },
      ac.signal,
    );
    expect(r.ok).toBe(false);
    expect(r.summary).toBe("aborted");
    expect(alive("sleep 700[34]")).toBe(false);
  }, 20_000);

  test("a background child left behind is swept", async () => {
    const h = await harness();
    const r = await h.run("run_command", {
      command: "sleep 7005 >/dev/null 2>&1 & echo started",
    });
    expect(h.text(r)).toContain("started");
    expect(alive("sleep 700[5]")).toBe(false);
  }, 20_000);

  test("keeps the tail with a notice and a full log in scratch", async () => {
    const h = await harness();
    const r = await h.run("run_command", { command: "seq 1 5000" });
    const text = h.text(r);
    expect(r.truncated).toBe(true);
    expect(text).toContain("5000\n");
    expect(text).not.toMatch(/^1\n/);
    expect(text).toContain("[Output truncated: showing the last 2000 lines");
    const log = /Full output \(scratch, outside the workspace\): (\S+)\]/.exec(
      text,
    )?.[1];
    expect(log).toBeDefined();
    const full = readFileSync(log as string, "utf-8");
    expect(full.startsWith("1\n2\n")).toBe(true);
    expect(full.trimEnd().endsWith("5000")).toBe(true);
    expect(log?.startsWith(join(h.root))).toBe(false);
  });

  test("a very large stream is cut to 50 KB and still keeps a full log", async () => {
    const h = await harness({ limits: { commandMaxLogBytes: 400_000 } });
    const r = await h.run("run_command", {
      command: "yes abcdefghij | head -c 1000000",
    });
    const text = h.text(r);
    expect(r.truncated).toBe(true);
    expect(Buffer.byteLength(text)).toBeLessThan(50 * 1024 + 600);
    const log = /scratch, outside the workspace\): (\S+)\]/.exec(
      text,
    )?.[1] as string;
    expect(readFileSync(log).length).toBeLessThanOrEqual(400_000);
  });

  test("strips secret-looking env vars, keeps PATH, HOME, LANG, TERM", async () => {
    const h = await harness();
    process.env["AGENTKIT_TEST_TOKEN"] = "tok-123";
    process.env["MY_API_KEY"] = "key-456";
    process.env["DB_PASSWORD"] = "pw-789";
    process.env["SOME_CREDENTIAL"] = "cred-000";
    process.env["AGENTKIT_TEST_SECRET"] = "sec-111";
    try {
      const out = h.text(await h.run("run_command", { command: "env" }));
      for (const leaked of [
        "tok-123",
        "key-456",
        "pw-789",
        "cred-000",
        "sec-111",
      ])
        expect(out).not.toContain(leaked);
      for (const kept of ["PATH=", "HOME=", "LANG=", "TERM="])
        expect(out).toContain(kept);
    } finally {
      for (const k of [
        "AGENTKIT_TEST_TOKEN",
        "MY_API_KEY",
        "DB_PASSWORD",
        "SOME_CREDENTIAL",
        "AGENTKIT_TEST_SECRET",
      ])
        delete process.env[k];
    }
  });

  test("sanitizedEnv fills defaults and points TMPDIR at scratch", () => {
    const env = sanitizedEnv({ FOO: "1", GITHUB_TOKEN: "x" }, "/scratch");
    expect(env["FOO"]).toBe("1");
    expect(env["GITHUB_TOKEN"]).toBeUndefined();
    expect(env["PATH"]).toBe("/usr/bin:/bin:/usr/sbin:/sbin");
    expect(env["TERM"]).toBe("dumb");
    expect(env["TMPDIR"]).toBe("/scratch");
  });

  test("sanitizedEnv drops agent sockets, auth-ish names and credential URLs", () => {
    const env = sanitizedEnv(
      {
        SSH_AUTH_SOCK: "/tmp/agent.sock",
        GPG_AGENT_INFO: "x",
        MY_COOKIE: "c",
        SESSION_ID: "s",
        DB_PASSWD: "p",
        MY_PRIVATE_THING: "k",
        REMOTE: "https://user:pw@example.com/repo.git",
        PLAIN_URL: "https://example.com/repo.git",
        ALSO_OK: "ssh://git@example.com/x",
        LANG: "en_US.UTF-8",
        FOO: "1",
      },
      "/scratch",
    );
    for (const k of [
      "SSH_AUTH_SOCK",
      "GPG_AGENT_INFO",
      "MY_COOKIE",
      "SESSION_ID",
      "DB_PASSWD",
      "MY_PRIVATE_THING",
      "REMOTE",
    ])
      expect(env[k]).toBeUndefined();
    expect(env["PLAIN_URL"]).toBe("https://example.com/repo.git");
    expect(env["ALSO_OK"]).toBe("ssh://git@example.com/x");
    expect(env["LANG"]).toBe("en_US.UTF-8");
    expect(env["FOO"]).toBe("1");
  });

  test("killProcessTree on a dead pid does not throw", () => {
    expect(() => killProcessTree(2_000_000_000)).not.toThrow();
  });
});

describe("sandbox availability", () => {
  test("required + non-darwin platform gives shell_isolation_unavailable", async () => {
    const h = await harness({ sandbox: "required", platform: "linux" });
    const r = await h.run("run_command", { command: "echo hi" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("shell_isolation_unavailable");
    const t = await harness({
      sandbox: "required",
      platform: "linux",
      testCommands: ["echo hi"],
    });
    expect(t.text(await t.run("run_tests", { command: "echo hi" }))).toContain(
      "shell_isolation_unavailable",
    );
  });

  test("required + missing sandbox-exec gives shell_isolation_unavailable", async () => {
    const h = await harness({
      sandbox: "required",
      platform: "darwin",
      sandboxExecPath: "/nonexistent/sandbox-exec",
    });
    expect(
      h.text(await h.run("run_command", { command: "echo hi" })),
    ).toContain("shell_isolation_unavailable");
  });

  test("sandbox defaults to required", async () => {
    const root = tempDir();
    const h = await harness({ root, sandbox: undefined, platform: "linux" });
    expect(
      h.text(await h.run("run_command", { command: "echo hi" })),
    ).toContain("shell_isolation_unavailable");
  });

  test.skipIf(!darwin)(
    "a workspace path outside SAFE_PATH is refused (needs macOS)",
    async () => {
      const parent = tempDir();
      const root = join(parent, "my project");
      mkdirSync(root);
      const h = await harness({ root, sandbox: "required" });
      const r = await h.run("run_command", { command: "echo hi" });
      expect(h.text(r)).toContain("shell_isolation_unavailable");
      expect(h.text(r)).toContain("characters outside");
    },
  );
});

describe.skipIf(!darwin)(`run_command (macOS sandbox; ${REASON})`, () => {
  test("echo works and the cwd is the workspace", async () => {
    const h = await harness({ sandbox: "required" });
    const r = await h.run("run_command", { command: "echo sandboxed; pwd" });
    expect(r.ok).toBe(true);
    expect(h.text(r)).toBe(`sandboxed\n${h.root}\n[exit code 0]`);
    expect(r.warnings).toEqual([]);
  });

  test("writes inside the workspace work; outside fail", async () => {
    const h = await harness({ sandbox: "required" });
    const outside = tempDir("outside-");
    const target = join(outside, "pwned.txt");
    const r = await h.run("run_command", {
      command: `echo in > inside.txt && cat inside.txt; echo out > '${target}'`,
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("in\n");
    expect(h.text(r)).toContain("Operation not permitted");
    expect(existsSync(target)).toBe(false);
    expect(readFileSync(join(h.root, "inside.txt"), "utf-8")).toBe("in\n");
  });

  test("network is blocked, and allowed with network: true", async () => {
    const server = createServer((_req, res) => res.end("pong"));
    await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
    const { port } = server.address() as { port: number };
    try {
      const blocked = await harness({ sandbox: "required" });
      const no = await blocked.run("run_command", {
        command: `/usr/bin/curl -s --max-time 5 http://127.0.0.1:${port}/`,
      });
      expect(no.ok).toBe(false);
      expect(blocked.text(no)).not.toContain("pong");
      const open = await harness({ sandbox: "required", network: true });
      const yes = await open.run("run_command", {
        command: `/usr/bin/curl -s --max-time 5 http://127.0.0.1:${port}/`,
      });
      expect(open.text(yes)).toContain("pong");
    } finally {
      server.close();
    }
  }, 30_000);

  test("credentials and other temp dirs cannot be read", async () => {
    const h = await harness({ sandbox: "required" });
    const fakeHome = tempDir("fakehome-");
    put(fakeHome, ".ssh/id_ed25519", "PRIVATE-KEY-MATERIAL");
    put(fakeHome, "notes.txt", "OTHER-PROJECT");
    const r = await h.run("run_command", {
      command: `cat '${fakeHome}/.ssh/id_ed25519'; cat '${fakeHome}/notes.txt'; ls "$HOME/.ssh"; cat "$HOME/.ssh/config"`,
    });
    const text = h.text(r);
    expect(r.ok).toBe(false);
    expect(text).not.toContain("PRIVATE-KEY-MATERIAL");
    expect(text).not.toContain("OTHER-PROJECT");
    expect(
      text.match(/Operation not permitted/g)?.length,
    ).toBeGreaterThanOrEqual(3);
  });

  test("a timeout inside the sandbox kills both children", async () => {
    const h = await harness({ sandbox: "required" });
    const r = await h.run("run_command", {
      command: "sleep 7011 & sleep 7012 & wait",
      timeoutSec: 1,
    });
    expect(r.summary).toBe("timed out after 1s");
    expect(alive("sleep 701[12]")).toBe(false);
  }, 20_000);

  test("git hooks and config stay read-only so no hook can be planted", async () => {
    const h = await harness({ sandbox: "required" });
    mkdirSync(join(h.root, ".git", "hooks"), { recursive: true });
    put(h.root, ".git/config", "[core]\n");
    const r = await h.run("run_command", {
      command: "echo x > .git/hooks/pre-commit; echo y >> .git/config",
    });
    expect(r.ok).toBe(false);
    expect(h.text(r).match(/Operation not permitted/g)?.length).toBe(2);
    expect(existsSync(join(h.root, ".git", "hooks", "pre-commit"))).toBe(false);
  });

  test(".git is read-only by default and writable with allowGitWrites", async () => {
    const h = await harness({ sandbox: "required" });
    mkdirSync(join(h.root, ".git"), { recursive: true });
    const denied = await h.run("run_command", { command: "touch .git/x" });
    expect(denied.ok).toBe(false);
    expect(h.text(denied)).toContain("Operation not permitted");
    expect(existsSync(join(h.root, ".git", "x"))).toBe(false);

    const open = await harness({ sandbox: "required", allowGitWrites: true });
    mkdirSync(join(open.root, ".git", "hooks"), { recursive: true });
    put(open.root, ".git/config", "[core]\n");
    const ok = await open.run("run_command", { command: "touch .git/x" });
    expect(ok.ok).toBe(true);
    expect(existsSync(join(open.root, ".git", "x"))).toBe(true);
    const still = await open.run("run_command", {
      command: "echo y >> .git/config; touch .git/hooks/h",
    });
    expect(still.ok).toBe(false);
    expect(existsSync(join(open.root, ".git", "hooks", "h"))).toBe(false);
  });

  test("the node/bun toolchain still starts", async () => {
    const h = await harness({ sandbox: "required" });
    const r = await h.run("run_command", {
      command: `'${process.execPath}' --version`,
    });
    expect(r.ok).toBe(true);
  });
});

describe("run_tests", () => {
  test("runs an allowed command exactly as configured", async () => {
    const h = await harness({ testCommands: ["echo tests-ok"] });
    const r = await h.run("run_tests", { command: "echo tests-ok" });
    expect(r.ok).toBe(true);
    expect(h.text(r)).toBe("tests-ok\n[exit code 0]");
  });

  test("refuses anything that is not an exact configured command", async () => {
    const h = await harness({ testCommands: ["echo tests-ok"] });
    for (const command of [
      "echo tests-ok; id",
      "echo tests-ok ",
      "ECHO tests-ok",
      "rm -rf x",
      "bun test",
    ]) {
      const r = await h.run("run_tests", { command });
      expect(r.ok).toBe(false);
      expect(h.text(r)).toContain("test_command_not_allowed");
    }
    const none = await harness();
    expect(
      none.text(await none.run("run_tests", { command: "bun test" })),
    ).toContain("(none configured)");
  });

  test("a failing test command is ok:false with its output", async () => {
    const h = await harness({ testCommands: ["echo FAIL; exit 1"] });
    const r = await h.run("run_tests", { command: "echo FAIL; exit 1" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toBe("FAIL\n[exit code 1]");
  });
});
