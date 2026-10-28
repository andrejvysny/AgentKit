import { afterEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import {
  descendantsOf,
  parseEnviron,
  parsePsTable,
  parseExecId,
  parsePsOutput,
} from "../src/process-tree.js";
import { sweepOrphans } from "../src/process.js";
import { cleanup, harness } from "./support.js";

const posix = process.platform === "darwin" || process.platform === "linux";
const unix = test.skipIf(!posix);

const spawned: number[] = [];

afterEach(() => {
  for (const pid of spawned.splice(0)) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // already dead
    }
  }
  cleanup();
});

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function readPid(path: string): Promise<number> {
  for (let i = 0; i < 100 && !existsSync(path); i++)
    await new Promise((r) => setTimeout(r, 20));
  const pid = Number(readFileSync(path, "utf-8").trim());
  spawned.push(pid);
  return pid;
}

async function settled(pid: number): Promise<boolean> {
  for (let i = 0; i < 50; i++) {
    if (!alive(pid)) return true;
    await new Promise((r) => setTimeout(r, 20));
  }
  return false;
}

describe("process-tree parsers", () => {
  test("parseExecId", () => {
    expect(parseExecId("123:abc-1")).toEqual({
      ownerPid: 123,
      execId: "123:abc-1",
    });
    expect(parseExecId("abc:1")).toBeUndefined();
    expect(parseExecId("0:x")).toBeUndefined();
    expect(parseExecId("12")).toBeUndefined();
  });

  test("parsePsOutput finds the marker token among args and env", () => {
    const out = [
      "  101 /bin/sleep 300 HOME=/x AGENTKIT_EXEC_ID=42:u-1 TERM=dumb",
      "  102 /bin/ls AGENTKIT_EXEC_ID=bad",
      "  103 grep xAGENTKIT_EXEC_ID=7:z",
      "  104 /usr/bin/true AGENTKIT_EXEC_ID=9:k",
      "garbage",
    ].join("\n");
    expect(parsePsOutput(out)).toEqual([
      { pid: 101, execId: "42:u-1", ownerPid: 42 },
      { pid: 104, execId: "9:k", ownerPid: 9 },
    ]);
  });

  test("parsePsTable and descendantsOf follow parent links", () => {
    const rows = parsePsTable(
      [
        "    1     0 Mon Oct 27 08:00:00 2026",
        "  100     1 Mon Oct 27 09:00:00 2026",
        "  101   100 Mon Oct 27 09:00:01 2026",
        "  102   101 Mon Oct 27 09:00:02 2026",
        "  200     1 Mon Oct 27 09:00:03 2026",
        "junk",
      ].join("\n"),
    );
    expect(rows).toHaveLength(5);
    expect(rows[2]).toEqual({
      pid: 101,
      ppid: 100,
      started: "Mon Oct 27 09:00:01 2026",
    });
    expect(
      descendantsOf(rows, [100])
        .map((r) => r.pid)
        .sort(),
    ).toEqual([100, 101, 102]);
  });

  test("parseEnviron reads a NUL separated environment", () => {
    const raw = "A=1\0AGENTKIT_EXEC_ID=5:abc\0B=2\0";
    expect(parseEnviron(raw, 77)).toEqual({
      pid: 77,
      execId: "5:abc",
      ownerPid: 5,
    });
    expect(parseEnviron("A=1\0", 77)).toBeUndefined();
    expect(parseEnviron("AGENTKIT_EXEC_ID=nope\0", 77)).toBeUndefined();
  });
});

describe("tree teardown", () => {
  unix(
    "a descendant that left the process group is killed",
    async () => {
      const h = await harness();
      const pidFile = join(h.root, "escaped.pid");
      const r = await h.run("run_command", {
        command: `perl -e 'use POSIX; POSIX::setsid(); exec "sleep","300"' >/dev/null 2>&1 & echo $! > escaped.pid; sleep 0.4; echo done`,
      });
      const pid = await readPid(pidFile);
      expect(h.text(r)).toContain("done");
      expect(await settled(pid)).toBe(true);
    },
    20_000,
  );

  unix(
    "a double-forked daemon is killed",
    async () => {
      const h = await harness();
      const r = await h.run("run_command", {
        command: `(sleep 300 >/dev/null 2>&1 & echo $! > daemon.pid) & sleep 0.4`,
      });
      const pid = await readPid(join(h.root, "daemon.pid"));
      expect(r.ok).toBe(true);
      expect(await settled(pid)).toBe(true);
    },
    20_000,
  );

  unix(
    "a child that ignores SIGTERM is SIGKILLed after the grace",
    async () => {
      const h = await harness({ limits: { commandKillGraceMs: 300 } });
      const r = await h.run("run_command", {
        command: `trap "" TERM; sleep 300 & echo $! > stubborn.pid; wait`,
        timeoutSec: 1,
      });
      const pid = await readPid(join(h.root, "stubborn.pid"));
      expect(r.ok).toBe(false);
      expect(await settled(pid)).toBe(true);
      const teardown = (r.data as { teardown: unknown }).teardown;
      expect(teardown).toEqual({
        state: "stopped",
        escalated: true,
        survivors: [],
        scan: "ok",
      });
      expect(r.warnings).not.toContain(expect.stringContaining("survivors"));
    },
    20_000,
  );

  unix(
    "a graceful child exits on SIGTERM without escalation",
    async () => {
      const h = await harness();
      const r = await h.run("run_command", {
        command: `sleep 300 & echo $! > polite.pid; wait`,
        timeoutSec: 1,
      });
      const pid = await readPid(join(h.root, "polite.pid"));
      expect(r.ok).toBe(false);
      expect(await settled(pid)).toBe(true);
      expect((r.data as { teardown: unknown }).teardown).toEqual({
        state: "stopped",
        escalated: false,
        survivors: [],
        scan: "ok",
      });
    },
    20_000,
  );
});

describe("sweepOrphans", () => {
  unix(
    "kills a marked process whose owner is dead",
    async () => {
      const child = spawn(
        process.execPath,
        ["-e", "setTimeout(() => {}, 300000)"],
        {
          env: { ...process.env, AGENTKIT_EXEC_ID: "999999:orphan-test" },
          stdio: "ignore",
          detached: true,
        },
      );
      const pid = child.pid as number;
      spawned.push(pid);
      const exited = new Promise<void>((done) =>
        child.once("exit", () => done()),
      );
      await new Promise((r) => setTimeout(r, 200));
      expect(alive(pid)).toBe(true);
      await sweepOrphans({ graceMs: 500 });
      await Promise.race([exited, new Promise((r) => setTimeout(r, 2000))]);
      expect(alive(pid)).toBe(false);
    },
    20_000,
  );

  unix(
    "leaves a marked process whose owner is alive",
    async () => {
      const child = spawn(
        process.execPath,
        ["-e", "setTimeout(() => {}, 300000)"],
        {
          env: { ...process.env, AGENTKIT_EXEC_ID: `${process.pid}:live-test` },
          stdio: "ignore",
          detached: true,
        },
      );
      const pid = child.pid as number;
      spawned.push(pid);
      await new Promise((r) => setTimeout(r, 200));
      await sweepOrphans({ graceMs: 200 });
      expect(alive(pid)).toBe(true);
    },
    20_000,
  );
});
