/**
 * Cross-process workspace coordinator backed by a lock directory.
 *
 * Layout under `<dir>/<sha256(root)>/`:
 *   exclusive.lock           workspace-exclusive marker (command running)
 *   quarantine.lock          unresolved startup or legacy command ownership
 *   shared-<pid>-<uuid>.lock one per in-flight file lease
 *   file-<sha256(path)>.lock one per locked canonical file
 * Every lock file holds JSON `{pid, processStart, tool, label, createdAt}` and
 * is created atomically (written to a temp name, then `link()`ed into place,
 * which fails with EEXIST like O_EXCL but never exposes a half-written file).
 * `label` is only a 12-char hash of the holder's label: command text never
 * reaches the lock directory.
 *
 * Plain abandoned leases are reclaimed only by one elected reclaimer per
 * record generation; a hard-link receipt prevents stale readers from touching
 * newer ownership. Execution intent is fsynced into the existing exclusive
 * marker before spawn. Unknown/surviving executions and damaged records remain
 * blocked regardless of owner death. They are never released then quarantined.
 *
 * In-process fairness comes from the module-global in-process coordinator,
 * which is always acquired first, so packs using either kind of coordinator
 * in one process exclude each other.
 *
 * Limits: same host, OS user and pid namespace only. Reclaim receipts remain
 * as small safety records. Unknown inspection never proves cleanup. Historical
 * commands whose old leases were already released and whose marker/ancestry is
 * hidden cannot be reconstructed. An uncoordinated external editor
 * takes none of these locks; the tools' content checks detect its changes
 * instead.
 */
import { createHash, randomUUID } from "node:crypto";
import { link, mkdir, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  code,
  holderOf,
  liveOwner,
  ownerIsDead,
  processStart,
  readRecord,
  remove,
  type LockRecord,
  type OnError,
} from "./coordination-record.js";
import { executionLease } from "./coordination-execution.js";
import { importLegacyOwnership } from "./coordination-migration.js";
import { persistRecord } from "./coordination-storage.js";
import {
  type CoordinatorObserver,
  type ExecutionCleanup,
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

function sha(value: string): string {
  return createHash("sha256").update(value).digest("hex");
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
    lockId: randomUUID(),
    pid: process.pid,
    ...(start !== undefined ? { processStart: start } : {}),
    tool: holder.tool,
    label: sha(holder.label).slice(0, 12),
    createdAt: Date.now(),
  };
  const temp = join(dir, `.tmp-${randomUUID()}`);
  await writeFile(temp, JSON.stringify(record), { flag: "wx", mode: 0o600 });
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

  private async blocker(): Promise<LockRecord | undefined> {
    return (
      (await liveOwner(join(this.wsDir, "quarantine.lock"), this.onError)) ??
      (await liveOwner(this.marker, this.onError))
    );
  }

  async quarantine(cleanup: ExecutionCleanup): Promise<void> {
    await persistRecord(
      join(this.wsDir, "quarantine.lock"),
      {
        lockId: randomUUID(),
        pid: process.pid,
        tool: "startup_cleanup",
        label: "unresolved process cleanup",
        createdAt: Date.now(),
        execution: {
          runId: "startup",
          executionAttemptId: cleanup.executionAttemptId,
          execId: `${process.pid}:${cleanup.executionAttemptId}`,
          cleanup,
        },
      },
      true,
    );
  }

  async reconcile(): Promise<ExecutionCleanup[]> {
    const quarantine = await readRecord(join(this.wsDir, "quarantine.lock"));
    const record =
      quarantine === "gone" ? await readRecord(this.marker) : quarantine;
    if (record === "gone") return [];
    if (record === "corrupt")
      return [
        {
          executionAttemptId: "unreadable-workspace-owner",
          state: "unknown",
          survivors: [],
          reason: "workspace ownership is unreadable",
        },
      ];
    const execution = record.execution;
    if (!execution || execution.cleanup?.state === "stopped") return [];
    if (execution.cleanup) return [execution.cleanup];
    if (!(await ownerIsDead(record))) return [];
    return [
      {
        executionAttemptId: execution.executionAttemptId,
        state: "unknown",
        survivors: [],
        reason: "execution owner died before cleanup proof was persisted",
      },
    ];
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
      const first = await this.blocker();
      if (!first) {
        await tryCreate(this.wsDir, entry, holder, this.onError);
        // A marker may have appeared between the check and our entry.
        let second: LockRecord | undefined;
        try {
          second = await this.blocker();
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
    for (;;) {
      const quarantine = await liveOwner(
        join(this.wsDir, "quarantine.lock"),
        this.onError,
      );
      if (quarantine) {
        block("workspace", [holderOf(quarantine)]);
        await sleep(this.pollMs, signal);
        continue;
      }
      if (await tryCreate(this.wsDir, this.marker, holder, this.onError)) break;
      const owner = await liveOwner(this.marker, this.onError);
      if (!owner) continue;
      block("workspace", [holderOf(owner)]);
      await sleep(this.pollMs, signal);
    }
    try {
      for (;;) {
        const quarantine = await liveOwner(
          join(this.wsDir, "quarantine.lock"),
          this.onError,
        );
        if (quarantine) {
          block("workspace", [holderOf(quarantine)]);
          await sleep(this.pollMs, signal);
          continue;
        }
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

/** Attempts every release, then propagates failures to the cleanup boundary. */
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
      const failures: unknown[] = [];
      for (const path of paths) {
        try {
          await remove(path);
        } catch (error) {
          onError(error);
          failures.push(error);
        }
      }
      try {
        await inner.release();
      } catch (error) {
        onError(error);
        failures.push(error);
      }
      if (failures.length > 0)
        throw new AggregateError(failures, "workspace lease release failed");
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
  /** Observes cleanup errors, which also propagate to the caller. Default: `process.emitWarning`. */
  onError?: (error: unknown) => void;
  /** Profile-local lock namespaces from versions predating shared coordination. */
  legacyDirs?: readonly string[];
}): WorkspaceCoordinator {
  const inner = defaultInProcessCoordinator();
  const pollMs = options.pollMs ?? 50;
  const onError = options.onError ?? defaultOnError;

  async function open(root: string): Promise<{ key: string; dir: LockDir }> {
    const key = await canonicalRootKey(root);
    const wsDir = join(options.dir, sha(key));
    await mkdir(wsDir, { recursive: true, mode: 0o700 });
    await importLegacyOwnership(wsDir, sha(key), options.legacyDirs ?? []);
    return { key, dir: new LockDir(wsDir, pollMs, onError) };
  }

  return {
    async quarantineWorkspace(root, cleanup) {
      await (await open(root)).dir.quarantine(cleanup);
    },
    async reconcileWorkspace(root) {
      return (await open(root)).dir.reconcile();
    },
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
          return await executionLease(
            leaseOf(held, paths, onError),
            paths[0] as string,
          );
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

/** One same-host namespace across app data profiles, scoped to the OS user. */
export function defaultWorkspaceCoordinationDir(): string {
  return join(
    "/tmp",
    `agentkit-workspace-locks-${process.getuid?.() ?? "user"}`,
  );
}
