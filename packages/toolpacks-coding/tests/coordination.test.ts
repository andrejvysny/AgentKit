import { afterEach, describe, expect, test } from "bun:test";
import { type ChildProcess, spawn } from "node:child_process";
import {
  existsSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { executeToolSafely } from "../../core/src/runs/tool-exec.js";
import {
  type CoordinationWait,
  type LeaseHolder,
  createCodingToolPack,
  createInProcessWorkspaceCoordinator,
  createLockDirWorkspaceCoordinator,
} from "../src/index.js";
import { createIfAbsent } from "../src/fs-util.js";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

const read = (root: string, rel: string) =>
  readFileSync(join(root, rel), "utf-8");
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const holder = (tool: string, label = tool): LeaseHolder => ({
  runId: "r",
  tool,
  label,
});
let n = 0;
const chat = (name: string) => `${name}-${++n}-${Date.now()}`;

/** Resolves true if `p` settles within `ms`, false otherwise. */
async function settlesWithin(
  p: Promise<unknown>,
  ms: number,
): Promise<boolean> {
  const timeout = sleep(ms).then(() => false);
  return Promise.race([
    p.then(
      () => true,
      () => true,
    ),
    timeout,
  ]);
}

describe("two sessions, one workspace", () => {
  test("different files edited concurrently both succeed", async () => {
    const root = tempDir();
    const a = await harness({ root, chatId: chat("a") });
    const b = await harness({ root, chatId: chat("b") });
    put(root, "a.txt", "a1\n");
    put(root, "b.txt", "b1\n");
    await a.run("read_file", { path: "a.txt" });
    await b.run("read_file", { path: "b.txt" });
    const [ra, rb] = await Promise.all([
      a.run("edit_file", {
        path: "a.txt",
        edits: [{ oldText: "a1", newText: "a2" }],
      }),
      b.run("edit_file", {
        path: "b.txt",
        edits: [{ oldText: "b1", newText: "b2" }],
      }),
    ]);
    expect(ra.ok).toBe(true);
    expect(rb.ok).toBe(true);
    expect(read(root, "a.txt")).toBe("a2\n");
    expect(read(root, "b.txt")).toBe("b2\n");
  });

  test("both read v1, both edit: one wins, the other gets file_changed", async () => {
    const root = tempDir();
    const a = await harness({ root, chatId: chat("a") });
    const b = await harness({ root, chatId: chat("b") });
    put(root, "f.txt", "one\ntwo\n");
    await a.run("read_file", { path: "f.txt" });
    await b.run("read_file", { path: "f.txt" });
    const results = await Promise.all([
      a.run("edit_file", {
        path: "f.txt",
        edits: [{ oldText: "one", newText: "ONE-A" }],
      }),
      b.run("edit_file", {
        path: "f.txt",
        edits: [{ oldText: "two", newText: "TWO-B" }],
      }),
    ]);
    expect(results.filter((r) => r.ok)).toHaveLength(1);
    const loserIndex = results.findIndex((r) => !r.ok);
    const loser = results[loserIndex];
    expect(loser && a.text(loser)).toContain("file_changed");
    const winner = loserIndex === 0 ? "one\nTWO-B\n" : "ONE-A\ntwo\n";
    expect(read(root, "f.txt")).toBe(winner);
  });

  test("a session may chain its own edits without re-reading", async () => {
    const h = await harness({ chatId: chat("chain") });
    put(h.root, "f.txt", "x\ny\n");
    await h.run("read_file", { path: "f.txt" });
    const one = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "x", newText: "X" }],
    });
    const two = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "y", newText: "Y" }],
    });
    expect(one.ok && two.ok).toBe(true);
    expect(read(h.root, "f.txt")).toBe("X\nY\n");
  });

  test("an external write between read and edit is file_changed", async () => {
    const h = await harness({ chatId: chat("ext") });
    put(h.root, "f.txt", "v1\n");
    await h.run("read_file", { path: "f.txt" });
    writeFileSync(join(h.root, "f.txt"), "external\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "external", newText: "mine" }],
    });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("file_changed");
    expect(h.text(r)).toContain("Read it again and redo the edit.");
    expect(read(h.root, "f.txt")).toBe("external\n");
  });

  test("an invalid expectedSha256 is rejected", async () => {
    const h = await harness();
    put(h.root, "f.txt", "v1\n");
    const r = await h.run("edit_file", {
      path: "f.txt",
      expectedSha256: "nothex",
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    expect(h.text(r)).toContain("invalid_arguments");
  });
});

describe("write_file versioning", () => {
  test("overwriting an unread existing file is file_not_read", async () => {
    const h = await harness({ chatId: chat("w") });
    put(h.root, "f.txt", "old\n");
    const r = await h.run("write_file", { path: "f.txt", content: "new\n" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain(
      "file_not_read: f.txt already exists. Read it before overwriting it.",
    );
    expect(read(h.root, "f.txt")).toBe("old\n");
    await h.run("read_file", { path: "f.txt" });
    const ok = await h.run("write_file", { path: "f.txt", content: "new\n" });
    expect(ok.ok).toBe(true);
    expect((ok.data as { sha256: string }).sha256).toHaveLength(64);
  });

  test("a stale read is file_changed", async () => {
    const h = await harness({ chatId: chat("w") });
    put(h.root, "f.txt", "old\n");
    await h.run("read_file", { path: "f.txt" });
    writeFileSync(join(h.root, "f.txt"), "external\n");
    const r = await h.run("write_file", { path: "f.txt", content: "new\n" });
    expect(h.text(r)).toContain("file_changed");
    expect(read(h.root, "f.txt")).toBe("external\n");
  });

  test("two creators of one new file: one wins, the file holds its content", async () => {
    const root = tempDir();
    const a = await harness({ root, chatId: chat("a") });
    const b = await harness({ root, chatId: chat("b") });
    const [ra, rb] = await Promise.all([
      a.run("write_file", { path: "n.txt", content: "A\n" }),
      b.run("write_file", { path: "n.txt", content: "B\n" }),
    ]);
    expect([ra.ok, rb.ok].filter(Boolean)).toHaveLength(1);
    const loser = ra.ok ? rb : ra;
    // Serialised by the shared lock, the loser sees an existing unread file.
    expect(a.text(loser)).toContain("file_not_read");
    expect(read(root, "n.txt")).toBe(ra.ok ? "A\n" : "B\n");
  });

  test("create-if-absent refuses a file that appeared first", async () => {
    const root = tempDir();
    const path = join(root, "n.txt");
    await createIfAbsent(path, Buffer.from("first\n"), "n.txt");
    await expect(
      createIfAbsent(path, Buffer.from("second\n"), "n.txt"),
    ).rejects.toMatchObject({ code: "file_changed" });
    expect(read(root, "n.txt")).toBe("first\n");
  });
});

describe("in-process coordinator", () => {
  test("a symlink alias serialises on the same file lock", async () => {
    const root = tempDir();
    put(root, "real/f.txt", "x");
    symlinkSync(join(root, "real"), join(root, "alias"));
    const waits: CoordinationWait[] = [];
    const c = createInProcessWorkspaceCoordinator({
      onWaiting: (w) => waits.push(w),
    });
    const first = await c.acquireFile({
      root,
      path: join(root, "real/f.txt"),
      holder: holder("one"),
    });
    let second = false;
    const pending = c
      .acquireFile({
        root,
        path: join(root, "alias/f.txt"),
        holder: holder("two"),
      })
      .then((l) => {
        second = true;
        return l;
      });
    await sleep(50);
    expect(second).toBe(false);
    expect(waits).toHaveLength(1);
    expect(waits[0]?.kind).toBe("file");
    expect(waits[0]?.holders[0]?.tool).toBe("one");
    await first.release();
    await first.release();
    await (await pending).release();
    expect(second).toBe(true);
  });

  test("workspace lease waits for files, blocks newcomers, FIFO", async () => {
    const root = tempDir();
    const order: string[] = [];
    const c = createInProcessWorkspaceCoordinator();
    const file1 = await c.acquireFile({
      root,
      path: join(root, "a"),
      holder: holder("f1"),
    });
    const ws = c.acquireWorkspace({ root, holder: holder("ws") }).then((l) => {
      order.push("ws");
      return l;
    });
    await sleep(20);
    const file2 = c
      .acquireFile({ root, path: join(root, "b"), holder: holder("f2") })
      .then((l) => {
        order.push("f2");
        return l;
      });
    await sleep(40);
    expect(order).toEqual([]);
    await file1.release();
    const wsLease = await ws;
    await sleep(40);
    expect(order).toEqual(["ws"]);
    await wsLease.release();
    await (await file2).release();
    expect(order).toEqual(["ws", "f2"]);
  });

  test("abort while waiting rejects and leaves no lease behind", async () => {
    const root = tempDir();
    const c = createInProcessWorkspaceCoordinator();
    const ws = await c.acquireWorkspace({ root, holder: holder("ws") });
    const ac = new AbortController();
    const waiting = c.acquireFile({
      root,
      path: join(root, "f"),
      holder: holder("f"),
      signal: ac.signal,
    });
    await sleep(20);
    ac.abort();
    await expect(waiting).rejects.toMatchObject({ name: "AbortError" });
    await ws.release();
    const again = await Promise.race([
      c.acquireWorkspace({ root, holder: holder("ws2") }),
      sleep(500).then(() => undefined),
    ]);
    expect(again).toBeDefined();
    await again?.release();
  });
});

describe("shell lease", () => {
  test("blocks a native edit until the command ends; reads never wait", async () => {
    const waits: CoordinationWait[] = [];
    const coordinator = createInProcessWorkspaceCoordinator({
      onWaiting: (w) => waits.push(w),
    });
    const root = tempDir();
    const shell = await harness({ root, coordinator, chatId: chat("sh") });
    const editor = await harness({ root, coordinator, chatId: chat("ed") });
    put(root, "f.txt", "v1\n");
    await editor.run("read_file", { path: "f.txt" });
    const command = shell.run("run_command", {
      command: "touch started; sleep 0.6; echo done > finished",
    });
    while (!existsSync(join(root, "started"))) await sleep(10);
    const edit = editor.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    const readDuring = await editor.run("read_file", { path: "f.txt" });
    expect(readDuring.ok).toBe(true);
    expect(existsSync(join(root, "finished"))).toBe(false);
    expect(read(root, "f.txt")).toBe("v1\n");
    expect(waits).toHaveLength(1);
    expect(waits[0]?.kind).toBe("workspace");
    expect(waits[0]?.waiter.tool).toBe("edit_file");
    expect(waits[0]?.holders[0]?.tool).toBe("run_command");
    expect(waits[0]?.holders[0]?.label).toContain("sleep 0.6");
    expect((await command).ok).toBe(true);
    expect((await edit).ok).toBe(true);
    expect(existsSync(join(root, "finished"))).toBe(true);
    expect(read(root, "f.txt")).toBe("v2\n");
  });

  test("every command waits, even plain git inspection", async () => {
    const coordinator = createInProcessWorkspaceCoordinator();
    const root = tempDir();
    const h = await harness({ root, coordinator });
    const held = await coordinator.acquireWorkspace({
      root,
      holder: holder("other"),
    });
    const blocked = h.run("run_command", { command: "git log '--output=x'" });
    expect(await settlesWithin(blocked, 150)).toBe(false);
    await held.release();
    await blocked;
  });

  test("a busy workspace fails with workspace_busy naming the holder", async () => {
    const coordinator = createInProcessWorkspaceCoordinator();
    const root = tempDir();
    const h = await harness({ root, coordinator, coordinationWaitMs: 150 });
    const held = await coordinator.acquireWorkspace({
      root,
      holder: holder("run_command", "bun test"),
    });
    const r = await h.run("run_command", { command: "touch ran" });
    expect(r.ok).toBe(false);
    expect(h.text(r)).toContain("workspace_busy");
    expect(h.text(r)).toContain(
      "another session is running run_command: bun test",
    );
    expect(existsSync(join(root, "ran"))).toBe(false);
    await held.release();
  });

  test("coordinationWaitMs must stay below the tool timeout", async () => {
    await expect(
      harness({
        coordinationWaitMs: 60_000,
        limits: { toolTimeoutMs: 30_000 },
      }),
    ).rejects.toThrow(TypeError);
  });

  test("cancel while waiting for the lease spawns nothing", async () => {
    const coordinator = createInProcessWorkspaceCoordinator();
    const root = tempDir();
    const h = await harness({ root, coordinator });
    const held = await coordinator.acquireWorkspace({
      root,
      holder: holder("other"),
    });
    const ac = new AbortController();
    const run = h.run("run_command", { command: "touch spawned" }, ac.signal);
    await sleep(50);
    ac.abort();
    const result = await run;
    expect(result.ok).toBe(false);
    expect(h.text(result)).toContain("aborted");
    await held.release();
    await sleep(100);
    expect(existsSync(join(root, "spawned"))).toBe(false);
  });
});

describe("partial reads and path identity", () => {
  test("a partial read does not authorise an overwrite but anchors an edit", async () => {
    const h = await harness({ chatId: chat("part") });
    put(h.root, "f.txt", "a\nb\nc\n");
    await h.run("read_file", { path: "f.txt", offset: 2 });
    const w = await h.run("write_file", { path: "f.txt", content: "new\n" });
    expect(h.text(w)).toContain("file_not_read");
    expect(h.text(w)).toContain("only read part");
    await h.run("read_file", { path: "f.txt", limit: 1 });
    expect(
      h.text(await h.run("write_file", { path: "f.txt", content: "new\n" })),
    ).toContain("file_not_read");
    const e = await h.run("edit_file", {
      path: "f.txt",
      edits: [{ oldText: "b", newText: "B" }],
    });
    expect(e.ok).toBe(true);
    // The edit was the session's own write: it now holds the full version.
    expect(
      (await h.run("write_file", { path: "f.txt", content: "new\n" })).ok,
    ).toBe(true);
  });

  test("a read truncated by the line cap is partial", async () => {
    const h = await harness({
      chatId: chat("trunc"),
      limits: { readMaxLines: 2 },
    });
    put(h.root, "f.txt", "a\nb\nc\n");
    await h.run("read_file", { path: "f.txt" });
    expect(
      h.text(await h.run("write_file", { path: "f.txt", content: "x" })),
    ).toContain("file_not_read");
    put(h.root, "g.txt", "a\nb\n");
    await h.run("read_file", { path: "g.txt" });
    expect(
      (await h.run("write_file", { path: "g.txt", content: "x" })).ok,
    ).toBe(true);
  });

  test("an explicit expectedSha256 for a missing file does not create it", async () => {
    const h = await harness();
    const r = await h.run("write_file", {
      path: "gone.txt",
      content: "x",
      expectedSha256: "a".repeat(64),
    });
    expect(h.text(r)).toContain("file_changed");
    expect(existsSync(join(h.root, "gone.txt"))).toBe(false);
  });

  test("a parent swapped for a symlink after locking is file_changed", async () => {
    const root = tempDir();
    put(root, "d/f.txt", "v1\n");
    put(root, "other/f.txt", "v1\n");
    const coordinator = createInProcessWorkspaceCoordinator();
    const slow = await harness({ root, coordinator, chatId: chat("swap2") });
    await slow.run("read_file", { path: "d/f.txt" });
    const lease = await coordinator.acquireWorkspace({
      root,
      holder: holder("blocker"),
    });
    const edit = slow.run("edit_file", {
      path: "d/f.txt",
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    await sleep(50);
    rmSync(join(root, "d"), { recursive: true });
    symlinkSync(join(root, "other"), join(root, "d"));
    await lease.release();
    const r = await edit;
    // Locked as d/f.txt, now resolves to other/f.txt: refused, nothing written.
    expect(r.ok).toBe(false);
    expect(slow.text(r)).toContain("file_changed");
    expect(read(root, "other/f.txt")).toBe("v1\n");
  });

  test("a write blocked behind a lease never lands after its signal aborts", async () => {
    const coordinator = createInProcessWorkspaceCoordinator();
    const root = tempDir();
    const h = await harness({ root, coordinator, chatId: chat("late") });
    put(root, "f.txt", "v1\n");
    await h.run("read_file", { path: "f.txt" });
    const lease = await coordinator.acquireWorkspace({
      root,
      holder: holder("blocker"),
    });
    const ac = new AbortController();
    const edit = h.run(
      "edit_file",
      { path: "f.txt", edits: [{ oldText: "v1", newText: "v2" }] },
      ac.signal,
    );
    await sleep(30);
    ac.abort();
    expect((await edit).ok).toBe(false);
    await lease.release();
    await sleep(100);
    expect(read(root, "f.txt")).toBe("v1\n");
  });

  test("the core tool timeout aborts the signal handed to the tool", async () => {
    const coordinator = createInProcessWorkspaceCoordinator();
    const root = tempDir();
    const pack = createCodingToolPack({
      workspace: { root },
      sandbox: "off",
      coordinator,
      coordinationWaitMs: 400,
      limits: { toolTimeoutMs: 600 },
    });
    const tools = await pack.contribute({
      bindings: [],
      limits: { profile: "small", maxBytes: 1 << 20 },
    });
    const edit = tools.find((t) => t.definition.name === "edit_file");
    if (!edit) throw new Error("no edit_file");
    put(root, "f.txt", "v1\n");
    const lease = await coordinator.acquireWorkspace({
      root,
      holder: holder("blocker"),
    });
    const outcome = await executeToolSafely(
      edit,
      {
        runId: "t",
        bindings: [],
        limits: { profile: "small", maxBytes: 1 << 20 },
        chatId: chat("timeout"),
      },
      { path: "f.txt", edits: [{ oldText: "v1", newText: "v2" }] },
    );
    await lease.release();
    await sleep(100);
    expect(read(root, "f.txt")).toBe("v1\n");
    // Either the wait cap (workspace_busy) or the timeout ended the call.
    expect(outcome.ok ? (outcome.value as { ok: boolean }).ok : false).toBe(
      false,
    );
  });

  test("case-different spellings serialise on a case-insensitive volume", async () => {
    const root = tempDir();
    put(root, "foo.ts", "x");
    const insensitive = existsSync(join(root, "FOO.ts"));
    if (!insensitive) return;
    const c = createInProcessWorkspaceCoordinator();
    const first = await c.acquireFile({
      root,
      path: join(root, "foo.ts"),
      holder: holder("one"),
    });
    let second = false;
    const pending = c
      .acquireFile({ root, path: join(root, "FOO.ts"), holder: holder("two") })
      .then((l) => {
        second = true;
        return l;
      });
    await sleep(50);
    expect(second).toBe(false);
    await first.release();
    await (await pending).release();
  });
});

describe("lock-dir coordinator (cross-process)", () => {
  const children: ChildProcess[] = [];
  afterEach(() => {
    for (const child of children.splice(0)) child.kill("SIGKILL");
  });

  function spawnHolder(
    dir: string,
    root: string,
    mode: "file" | "workspace",
  ): Promise<ChildProcess> {
    const child = spawn(
      process.execPath,
      [join(import.meta.dir, "fixtures/lock-holder.ts"), dir, root, mode],
      { stdio: ["pipe", "pipe", "inherit"] },
    );
    children.push(child);
    return new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("exit", (code) => reject(new Error(`child exited ${code}`)));
      child.stdout?.on("data", (d) => {
        if (String(d).includes("ready")) resolve(child);
      });
    });
  }

  test("exclusive waits for another process's shared lease", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const child = await spawnHolder(dir, root, "file");
    const waits: CoordinationWait[] = [];
    const c = createLockDirWorkspaceCoordinator({
      dir,
      pollMs: 20,
      observer: { onWaiting: (w) => waits.push(w) },
    });
    let got = false;
    const pending = c
      .acquireWorkspace({ root, holder: holder("cmd") })
      .then((l) => {
        got = true;
        return l;
      });
    await sleep(300);
    expect(got).toBe(false);
    expect(waits).toHaveLength(1);
    expect(waits[0]?.holders[0]?.tool).toBe("child");
    child.stdin?.write("release\n");
    const lease = await pending;
    expect(got).toBe(true);
    await lease.release();
  });

  test("a file lease waits for another process's exclusive lease", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const child = await spawnHolder(dir, root, "workspace");
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    let got = false;
    const pending = c
      .acquireFile({ root, path: join(root, "f.txt"), holder: holder("edit") })
      .then((l) => {
        got = true;
        return l;
      });
    await sleep(200);
    expect(got).toBe(false);
    child.stdin?.write("release\n");
    await (await pending).release();
    expect(got).toBe(true);
  });

  test("a lock left by a dead process is reclaimed", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const child = await spawnHolder(dir, root, "workspace");
    child.kill("SIGKILL");
    await new Promise((r) => child.once("exit", r));
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const lease = await Promise.race([
      c.acquireWorkspace({ root, holder: holder("cmd") }),
      sleep(3000).then(() => undefined),
    ]);
    expect(lease).toBeDefined();
    await lease?.release();
  });

  test("abort while waiting on another process leaves no marker", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const child = await spawnHolder(dir, root, "file");
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const ac = new AbortController();
    const pending = c.acquireWorkspace({
      root,
      holder: holder("cmd"),
      signal: ac.signal,
    });
    await sleep(100);
    ac.abort();
    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
    child.stdin?.write("release\n");
    await new Promise((r) => child.once("exit", r));
    const lease = await Promise.race([
      c.acquireWorkspace({ root, holder: holder("again") }),
      sleep(2000).then(() => undefined),
    ]);
    expect(lease).toBeDefined();
    await lease?.release();
  });
});
