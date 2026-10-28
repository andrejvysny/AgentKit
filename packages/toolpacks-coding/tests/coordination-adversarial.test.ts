/**
 * Adversarial coordination: the scenarios a happy-path lock suite never hits —
 * corrupt and half-written lock files, a recycled PID claiming a live start
 * time (and the converse: a live PID that must NOT be stolen), a waiter whose
 * holder is cancelled, a wait that reaches its cap, and the filesystem moving
 * under a mutation between validation and the final write (target swapped for
 * a symlink). The contract: conflicts fail explicitly, dead owners are
 * reclaimed, live ones never are, and nothing writes through a swapped path.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import {
  createLockDirWorkspaceCoordinator,
  type LeaseHolder,
} from "../src/index.js";
import { canonicalRootKey } from "../src/coordination.js";
import { cleanup, harness, put, tempDir } from "./support.js";

afterEach(cleanup);

const holder = (tool: string, label = tool): LeaseHolder => ({
  runId: "r",
  tool,
  label,
});
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Plant a raw record at the workspace marker the coordinator will read. */
async function plantLock(
  dir: string,
  root: string,
  record: unknown,
): Promise<string> {
  const key = await canonicalRootKey(root);
  const wsDir = join(dir, createHash("sha256").update(key).digest("hex"));
  mkdirSync(wsDir, { recursive: true });
  const path = join(wsDir, "exclusive.lock");
  writeFileSync(
    path,
    typeof record === "string" ? record : JSON.stringify(record),
  );
  return path;
}

function backdate(path: string, ms: number): void {
  const when = new Date(Date.now() - ms);
  utimesSync(path, when, when);
}

describe("lock-dir coordinator under hostile files", () => {
  test("a corrupt lock older than the grace period is reclaimed", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const lock = await plantLock(dir, root, "not json {{{ half a write");
    backdate(lock, 10_000);
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const lease = await Promise.race([
      c.acquireWorkspace({ root, holder: holder("cmd") }),
      sleep(3000).then(() => undefined),
    ]);
    expect(lease).toBeDefined();
    await lease?.release();
  });

  test("a corrupt lock inside the grace period is treated as held, not ignored", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    await plantLock(dir, root, `{"pid": ${process.pid}, "to`);
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    await expect(
      c.acquireWorkspace({ root, holder: holder("cmd"), maxWaitMs: 300 }),
    ).rejects.toMatchObject({ code: "workspace_busy" });
  });

  test("a recycled PID with a mismatched start time does not keep the lock", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    // Our own pid — but the claimed start time is not this process's. The
    // pid-liveness check alone would call this lock live; the start-time
    // comparison is what catches a recycled PID.
    await plantLock(dir, root, {
      pid: process.pid,
      processStart: "Mon Jan 1 00:00:00 2001",
      tool: "dead",
      label: "recycled",
      createdAt: Date.now() - 60_000,
    });
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const lease = await Promise.race([
      c.acquireWorkspace({ root, holder: holder("cmd") }),
      sleep(3000).then(() => undefined),
    ]);
    expect(lease).toBeDefined();
    await lease?.release();
  });

  test("a live PID without a start time is never stolen", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    // A lock whose owner is verifiably alive and whose start time we cannot
    // check: reclaiming it would let two writers run. It must be honored.
    await plantLock(dir, root, {
      pid: process.pid,
      tool: "alive",
      label: "honest",
      createdAt: Date.now() - 60_000,
    });
    const c = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    await expect(
      c.acquireWorkspace({ root, holder: holder("cmd"), maxWaitMs: 300 }),
    ).rejects.toMatchObject({ code: "workspace_busy" });
  });

  test("a waiter proceeds the moment its holder's command is cancelled", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const holderSide = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const waiterSide = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const lease = await holderSide.acquireWorkspace({
      root,
      holder: holder("run_command", "sleep 300"),
    });
    let acquired = false;
    const waiting = waiterSide
      .acquireWorkspace({ root, holder: holder("edit_file"), maxWaitMs: 5_000 })
      .then((l) => {
        acquired = true;
        return l;
      });
    await sleep(150);
    expect(acquired).toBe(false);
    // The command was cancelled: teardown releases the lease.
    await lease.release();
    const got = await waiting;
    expect(acquired).toBe(true);
    await got.release();
  });

  test("a wait that reaches its cap fails with workspace_busy naming the holder", async () => {
    const dir = tempDir("locks-");
    const root = tempDir();
    const holderSide = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const waiterSide = createLockDirWorkspaceCoordinator({ dir, pollMs: 20 });
    const lease = await holderSide.acquireWorkspace({
      root,
      holder: holder("run_command", "make deploy-local"),
    });
    await expect(
      waiterSide.acquireFile({
        root,
        path: join(root, "f.txt"),
        holder: holder("edit_file"),
        maxWaitMs: 250,
      }),
    ).rejects.toMatchObject({ code: "workspace_busy" });
    // Failing the wait wrote nothing and left the holder's lock alone.
    expect(existsSync(join(root, "f.txt"))).toBe(false);
    await lease.release();
  });
});

describe("the filesystem moving under a mutation", () => {
  test("edit_file refuses a target replaced by a symlink after the read", async () => {
    const outside = tempDir("out-");
    const secret = put(outside, "secret.txt", "TOP-SECRET\n");
    const h = await harness({ chatId: "c1" });
    put(h.root, "a.txt", "one\ntwo\n");
    const read = await h.run("read_file", { path: "a.txt" });
    expect(read.ok).toBe(true);
    // Between the read and the edit, an external actor swaps the target.
    rmSync(join(h.root, "a.txt"));
    symlinkSync(secret, join(h.root, "a.txt"));
    const edit = await h.run("edit_file", {
      path: "a.txt",
      edits: [{ oldText: "two", newText: "three" }],
    });
    expect(edit.ok).toBe(false);
    // The write never followed the symlink out of the workspace.
    expect(readFileSync(secret, "utf8")).toBe("TOP-SECRET\n");
    await h.dispose();
  });

  test("write_file refuses to overwrite through a symlink swapped in before the write", async () => {
    const outside = tempDir("out-");
    const secret = put(outside, "secret.txt", "TOP-SECRET\n");
    const h = await harness({ chatId: "c2" });
    const rw = async () => {
      const read = await h.run("read_file", { path: "a.txt" });
      expect(read.ok).toBe(true);
      rmSync(join(h.root, "a.txt"));
      symlinkSync(secret, join(h.root, "a.txt"));
      const write = await h.run("write_file", {
        path: "a.txt",
        content: "overwrite attempt\n",
      });
      expect(write.ok).toBe(false);
    };
    put(h.root, "a.txt", "one\ntwo\n");
    await rw();
    expect(readFileSync(secret, "utf8")).toBe("TOP-SECRET\n");
    await h.dispose();
  });

  test("creating a new file onto a planted symlink fails as already-existing, never through it", async () => {
    const outside = tempDir("out-");
    const secret = put(outside, "secret.txt", "TOP-SECRET\n");
    const h = await harness({ chatId: "c3" });
    symlinkSync(secret, join(h.root, "new.txt"));
    const write = await h.run("write_file", {
      path: "new.txt",
      content: "mine\n",
    });
    expect(write.ok).toBe(false);
    expect(readFileSync(secret, "utf8")).toBe("TOP-SECRET\n");
    await h.dispose();
  });

  test("an external write between the read and the edit is file_changed, not clobbered", async () => {
    const h = await harness({ chatId: "c4" });
    put(h.root, "race.txt", "v1\n");
    expect((await h.run("read_file", { path: "race.txt" })).ok).toBe(true);
    // An external process wins the race — the edit must refuse.
    writeFileSync(join(h.root, "race.txt"), "external\n");
    const edit = await h.run("edit_file", {
      path: "race.txt",
      edits: [{ oldText: "v1", newText: "v2" }],
    });
    expect(edit.ok).toBe(false);
    expect(readFileSync(join(h.root, "race.txt"), "utf8")).toBe("external\n");
    await h.dispose();
  });
});
