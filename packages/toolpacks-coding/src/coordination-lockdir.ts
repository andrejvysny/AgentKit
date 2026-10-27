/**
 * Cross-process workspace coordinator backed by a lock directory.
 *
 * Layout under `<dir>/<sha256(root)>/`:
 *   exclusive.lock           workspace-exclusive marker (command running)
 *   shared-<pid>-<uuid>.lock one per in-flight file lease
 *   file-<sha256(path)>.lock one per locked canonical file
 * Every lock file holds JSON `{pid, processStart, tool, label, createdAt}` and
 * is created atomically (written to a temp name, then `link()`ed into place,
 * which fails with EEXIST like O_EXCL but never exposes a half-written file).
 * `label` is only a 12-char hash of the holder's label: command text never
 * reaches the lock directory.
 *
 * A lock is stale when its pid is gone (`process.kill(pid, 0)` -> ESRCH) or
 * the pid now has a different start time (`ps -o lstart=`; pid reuse). A stale
 * lock is reclaimed by renaming it to a unique tombstone and re-checking the
 * tombstone's content before deleting it; if the tombstone turns out to hold a
 * live holder's lock, it is linked back (no overwrite) and treated as owned by
 * that holder.
 *
 * In-process fairness comes from the module-global in-process coordinator,
 * which is always acquired first, so packs using either kind of coordinator
 * in one process exclude each other.
 *
 * Limits: same host and same pid namespace only (pids and `ps` mean nothing
 * across machines or containers sharing the directory); if `ps` is
 * unavailable only pid liveness is checked; release failures are reported via
 * `onError` and never change a tool result. An uncoordinated external editor
 * takes none of these locks; the tools' content checks detect its changes
 * instead.
 */
import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  link,
  mkdir,
  readFile,
  readdir,
  rename,
  stat,
  unlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import {
  type CoordinatorObserver,
  type LeaseHolder,
  WaitTracker,
  type WorkspaceCoordinator,
  type WorkspaceLease,
  abortError,
  canonicalFileKey,
  canonicalRootKey,
  defaultInProcessCoordinator,
  withWaitCap,
} from "./coordination.js";

const runPs = promisify(execFile);

interface LockRecord {
  pid: number;
  /** `ps -o lstart=` of the owner when it took the lock; absent if unknown. */
  processStart?: string;
  tool: string;
  /** sha256(label).slice(0, 12). */
  label: string;
  createdAt: number;
}

type OnError = (error: unknown) => void;

/** An unparseable lock file this old is garbage, not a half-written lock. */
const CORRUPT_GRACE_MS = 5000;
const START_TTL_MS = 1000;

function code(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : undefined;
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return code(error) !== "ESRCH";
  }
}

function sha(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

const starts = new Map<number, { value: string | undefined; at: number }>();

/** Process start time as printed by ps; undefined when it cannot be read. */
async function processStart(pid: number): Promise<string | undefined> {
  const cached = starts.get(pid);
  const ttl = pid === process.pid ? Number.POSITIVE_INFINITY : START_TTL_MS;
  if (cached && Date.now() - cached.at < ttl) return cached.value;
  let value: string | undefined;
  try {
    const { stdout } = await runPs("ps", ["-o", "lstart=", "-p", String(pid)], {
      env: { ...process.env, LC_ALL: "C" },
    });
    value = stdout.trim() || undefined;
  } catch {
    value = undefined; // ps missing or pid gone: callers fall back to liveness
  }
  starts.set(pid, { value, at: Date.now() });
  return value;
}

async function isStale(record: LockRecord): Promise<boolean> {
  if (!pidAlive(record.pid)) return true;
  if (record.processStart === undefined) return false;
  const now = await processStart(record.pid);
  return now !== undefined && now !== record.processStart;
}

function holderOf(record: LockRecord): LeaseHolder {
  return {
    runId: `pid-${record.pid}`,
    tool: record.tool,
    label: record.label,
  };
}

const UNKNOWN: LockRecord = {
  pid: -1,
  tool: "unknown",
  label: "unreadable lock file",
  createdAt: 0,
};

function isRecord(value: unknown): value is LockRecord {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as LockRecord).pid === "number" &&
    typeof (value as LockRecord).tool === "string"
  );
}

async function remove(path: string): Promise<void> {
  try {
    await unlink(path);
  } catch (error) {
    if (code(error) !== "ENOENT") throw error;
  }
}

async function readRecord(
  path: string,
): Promise<LockRecord | "gone" | "corrupt"> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (error) {
    if (code(error) === "ENOENT") return "gone";
    throw error;
  }
  try {
    const parsed: unknown = JSON.parse(text);
    return isRecord(parsed) ? parsed : "corrupt";
  } catch {
    return "corrupt";
  }
}

function sameOwner(a: LockRecord, b: LockRecord): boolean {
  return a.pid === b.pid && a.processStart === b.processStart;
}

/**
 * Take a stale (or corrupt) lock out of the way. Returns the live record when
 * the file turned out to belong to someone else after all.
 */
async function reclaim(
  path: string,
  judged: LockRecord | "corrupt",
  onError: OnError,
): Promise<LockRecord | undefined> {
  const tomb = `${path}.stale-${randomUUID()}`;
  try {
    await rename(path, tomb);
  } catch (error) {
    if (code(error) === "ENOENT") return undefined;
    throw error;
  }
  const moved = await readRecord(tomb);
  const unchanged =
    moved === "gone" ||
    (judged === "corrupt"
      ? moved === "corrupt"
      : moved !== "corrupt" && sameOwner(moved, judged));
  if (unchanged) {
    await remove(tomb).catch(onError);
    return undefined;
  }
  // We displaced a different holder's lock. link() never overwrites, so put it
  // back if the name is still free; either way that holder owns the lock.
  await link(tomb, path).catch(onError);
  await remove(tomb).catch(onError);
  return moved === "corrupt" ? UNKNOWN : moved;
}

/** The live owner of a lock file, or undefined (also after reclaiming a stale one). */
async function liveOwner(
  path: string,
  onError: OnError,
): Promise<LockRecord | undefined> {
  const record = await readRecord(path);
  if (record === "gone") return undefined;
  if (record === "corrupt") {
    const info = await stat(path).catch(() => null);
    if (info && Date.now() - info.mtimeMs < CORRUPT_GRACE_MS) return UNKNOWN;
    return reclaim(path, "corrupt", onError);
  }
  if (!(await isStale(record))) return record;
  return reclaim(path, record, onError);
}

async function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) throw abortError(signal);
  await new Promise<void>((done, fail) => {
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      done();
    }, ms);
    const onAbort = (): void => {
      clearTimeout(timer);
      fail(abortError(signal));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/** Atomically create `path` with our record; false when it already exists. */
async function tryCreate(
  dir: string,
  path: string,
  holder: LeaseHolder,
  onError: OnError,
): Promise<boolean> {
  const start = await processStart(process.pid);
  const record: LockRecord = {
    pid: process.pid,
    ...(start !== undefined ? { processStart: start } : {}),
    tool: holder.tool,
    label: sha(holder.label).slice(0, 12),
    createdAt: Date.now(),
  };
  const temp = join(dir, `.tmp-${randomUUID()}`);
  await writeFile(temp, JSON.stringify(record), { flag: "wx" });
  try {
    await link(temp, path);
    return true;
  } catch (error) {
    if (code(error) === "EEXIST") return false;
    throw error;
  } finally {
    // After a successful link the lock is ours; a failed unlink is only litter.
    await remove(temp).catch(onError);
  }
}

type Block = (kind: "file" | "workspace", holders: LeaseHolder[]) => void;

class LockDir {
  constructor(
    private readonly wsDir: string,
    private readonly pollMs: number,
    private readonly onError: OnError,
  ) {}

  private get marker(): string {
    return join(this.wsDir, "exclusive.lock");
  }

  async acquireShared(
    holder: LeaseHolder,
    signal: AbortSignal | undefined,
    block: Block,
  ): Promise<string> {
    const entry = join(
      this.wsDir,
      `shared-${process.pid}-${randomUUID()}.lock`,
    );
    for (;;) {
      const first = await liveOwner(this.marker, this.onError);
      if (!first) {
        await tryCreate(this.wsDir, entry, holder, this.onError);
        // A marker may have appeared between the check and our entry.
        let second: LockRecord | undefined;
        try {
          second = await liveOwner(this.marker, this.onError);
        } catch (error) {
          await remove(entry).catch(this.onError);
          throw error;
        }
        if (!second) return entry;
        await remove(entry);
        block("workspace", [holderOf(second)]);
      } else block("workspace", [holderOf(first)]);
      await sleep(this.pollMs, signal);
    }
  }

  async acquireExclusive(
    holder: LeaseHolder,
    signal: AbortSignal | undefined,
    block: Block,
  ): Promise<string> {
    while (!(await tryCreate(this.wsDir, this.marker, holder, this.onError))) {
      const owner = await liveOwner(this.marker, this.onError);
      if (!owner) continue;
      block("workspace", [holderOf(owner)]);
      await sleep(this.pollMs, signal);
    }
    try {
      for (;;) {
        const live = await this.liveShared();
        if (live.length === 0) return this.marker;
        block("workspace", live.map(holderOf));
        await sleep(this.pollMs, signal);
      }
    } catch (error) {
      await remove(this.marker).catch(this.onError);
      throw error;
    }
  }

  async acquireFileLock(
    key: string,
    holder: LeaseHolder,
    signal: AbortSignal | undefined,
    block: Block,
  ): Promise<string> {
    const path = join(this.wsDir, `file-${sha(key)}.lock`);
    while (!(await tryCreate(this.wsDir, path, holder, this.onError))) {
      const owner = await liveOwner(path, this.onError);
      if (!owner) continue;
      block("file", [holderOf(owner)]);
      await sleep(this.pollMs, signal);
    }
    return path;
  }

  private async liveShared(): Promise<LockRecord[]> {
    const names = (await readdir(this.wsDir)).filter(
      (n) => n.startsWith("shared-") && n.endsWith(".lock"),
    );
    const live: LockRecord[] = [];
    for (const name of names) {
      const owner = await liveOwner(join(this.wsDir, name), this.onError);
      if (owner) live.push(owner);
    }
    return live;
  }
}

/** Removes each path on its own and always frees the in-process gate. */
function leaseOf(
  inner: WorkspaceLease,
  paths: string[],
  onError: OnError,
): WorkspaceLease {
  let done = false;
  return {
    async release() {
      if (done) return;
      done = true;
      for (const path of paths) {
        try {
          await remove(path);
        } catch (error) {
          onError(error);
        }
      }
      try {
        await inner.release();
      } catch (error) {
        onError(error);
      }
    },
  };
}

function defaultOnError(error: unknown): void {
  process.emitWarning(
    `lock-dir coordinator: ${error instanceof Error ? error.message : String(error)}`,
  );
}

export function createLockDirWorkspaceCoordinator(options: {
  dir: string;
  observer?: CoordinatorObserver;
  pollMs?: number;
  /** Receives non-fatal cleanup errors. Default: `process.emitWarning`. */
  onError?: (error: unknown) => void;
}): WorkspaceCoordinator {
  const inner = defaultInProcessCoordinator();
  const pollMs = options.pollMs ?? 50;
  const onError = options.onError ?? defaultOnError;

  async function open(root: string): Promise<{ key: string; dir: LockDir }> {
    const key = await canonicalRootKey(root);
    const wsDir = join(options.dir, sha(key));
    await mkdir(wsDir, { recursive: true });
    return { key, dir: new LockDir(wsDir, pollMs, onError) };
  }

  return {
    async acquireFile(input) {
      const tracker = new WaitTracker(options.observer);
      const lease = await withWaitCap(input, tracker, async (signal) => {
        const held = await inner.acquireFileTracked({
          ...input,
          signal,
          tracker,
        });
        const paths: string[] = [];
        try {
          const { key, dir } = await open(input.root);
          const path = await canonicalFileKey(input.path);
          const block: Block = (kind, holders) =>
            tracker.waiting({
              kind,
              root: key,
              ...(kind === "file" ? { path } : {}),
              waiter: input.holder,
              holders,
            });
          paths.push(await dir.acquireShared(input.holder, signal, block));
          paths.push(
            await dir.acquireFileLock(path, input.holder, signal, block),
          );
          return leaseOf(held, paths, onError);
        } catch (error) {
          await leaseOf(held, paths, onError).release();
          throw error;
        }
      });
      tracker.acquired();
      return lease;
    },
    async acquireWorkspace(input) {
      const tracker = new WaitTracker(options.observer);
      const lease = await withWaitCap(input, tracker, async (signal) => {
        const held = await inner.acquireWorkspaceTracked({
          ...input,
          signal,
          tracker,
        });
        const paths: string[] = [];
        try {
          const { key, dir } = await open(input.root);
          const block: Block = (kind, holders) =>
            tracker.waiting({
              kind,
              root: key,
              waiter: input.holder,
              holders,
            });
          paths.push(await dir.acquireExclusive(input.holder, signal, block));
          return leaseOf(held, paths, onError);
        } catch (error) {
          await leaseOf(held, paths, onError).release();
          throw error;
        }
      });
      tracker.acquired();
      return lease;
    },
  };
}
