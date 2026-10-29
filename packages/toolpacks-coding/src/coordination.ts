/**
 * Workspace coordination: who may change what, and when.
 *
 * Two kinds of lease share one workspace:
 *  - a FILE lease (shared workspace lease + exclusive lock on one canonical
 *    file) held by edit_file / write_file while they read-check-write;
 *  - a WORKSPACE lease (exclusive) held by a write-capable shell command, so
 *    a native edit cannot overlap it.
 * Waiting is FIFO with writer preference: once a workspace lease is requested
 * no new file lease starts ahead of it. Read-only tools never take a lease.
 *
 * Limitation: an uncoordinated external editor (an IDE, another tool) takes
 * none of these locks. Its changes are caught by the content checks the tools
 * make against the version the session read, not by locking.
 */
import { realpath } from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { CodingError } from "./errors.js";
import type { ProcessIdentity } from "./process-identity.js";
import type { WorkspaceRecoveryReport } from "./coordination-recovery-types.js";
export type {
  WorkspaceRecoveryReason,
  WorkspaceRecoveryRecord,
  WorkspaceRecoveryReport,
} from "./coordination-recovery-types.js";
import {
  createLockDirWorkspaceCoordinator,
  defaultWorkspaceCoordinationDir,
} from "./coordination-lockdir.js";

export interface LeaseHolder {
  chatId?: string;
  runId: string;
  toolCallId?: string;
  tool: string;
  label: string;
}

export interface CoordinationWait {
  kind: "file" | "workspace";
  root: string;
  path?: string;
  waiter: LeaseHolder;
  holders: LeaseHolder[];
}

export interface WorkspaceLease {
  /** Idempotent. */
  release(): Promise<void>;
}

/** One admitted shell invocation, distinct from authorization consumption. */
export interface ExecutionIntent {
  runId: string;
  toolCallId?: string;
  executionAttemptId: string;
  execId: string;
}

export interface ExecutionCleanup {
  executionAttemptId: string;
  state: "stopped" | "survivors" | "unknown";
  survivors: number[];
  reason?: string;
}

/** Durable command ownership lives in the exclusive marker itself. */
export interface ExecutionLease extends WorkspaceLease {
  beginExecution(intent: ExecutionIntent): Promise<void>;
  spawned(identity: {
    pid: number;
    startIdentity?: string;
    processStart?: string;
    guardianProtocol?: "agentkit-command-guardian-v1";
    admission?: "pending" | "released";
  }): Promise<void>;
  /** Persists that the guardian is about to receive admission. */
  admitted?(): Promise<void>;
  observed?(
    identities: ProcessIdentity[],
    complete: boolean,
    closed?: boolean,
    sealed?: boolean,
  ): Promise<void>;
  finishExecution(cleanup: ExecutionCleanup): Promise<void>;
}

export function isExecutionLease(
  lease: WorkspaceLease,
): lease is ExecutionLease {
  return (
    "beginExecution" in lease &&
    "spawned" in lease &&
    "finishExecution" in lease
  );
}

export interface WorkspaceCoordinator {
  /** Inspect durable ownership before a pack admits mutations. Never clears unknown state. */
  reconcileWorkspace?(root: string): Promise<ExecutionCleanup[]>;
  /** Explicit inspection-only recovery. Incomplete ownership evidence remains blocked. */
  recoverWorkspace?(root: string): Promise<WorkspaceRecoveryReport>;
  /** Persist a blocker before an uncertain startup cleanup can admit mutations. */
  quarantineWorkspace?(root: string, cleanup: ExecutionCleanup): Promise<void>;
  /** Shared workspace lease + exclusive lock on one canonical file. Waits while a workspace-exclusive lease is held or requested. */
  acquireFile(input: {
    root: string;
    path: string;
    holder: LeaseHolder;
    signal?: AbortSignal;
    /** Give up waiting after this long with a `workspace_busy` CodingError. */
    maxWaitMs?: number;
  }): Promise<WorkspaceLease>;
  /** Exclusive workspace lease for a write-capable command. Waits for in-flight file leases; blocks new ones (writer preference, FIFO). */
  acquireWorkspace(input: {
    root: string;
    holder: LeaseHolder;
    signal?: AbortSignal;
    maxWaitMs?: number;
  }): Promise<WorkspaceLease>;
}

export interface CoordinatorObserver {
  onWaiting?(wait: CoordinationWait): void;
  onAcquired?(wait: CoordinationWait): void;
}

function isMissing(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error.code === "ENOENT" || error.code === "ENOTDIR")
  );
}

/** realpath of the nearest existing ancestor + the not-yet-existing rest. */
async function realpathLoose(path: string): Promise<string> {
  const missing: string[] = [];
  let current = resolve(path);
  while (true) {
    try {
      return join(await realpath(current), ...missing);
    } catch (error) {
      if (!isMissing(error)) throw error;
      const parent = dirname(current);
      if (parent === current) return join(current, ...missing);
      missing.unshift(basename(current));
      current = parent;
    }
  }
}

export function canonicalRootKey(root: string): Promise<string> {
  return realpathLoose(root);
}

/**
 * Symlink aliases collapse: an existing target resolves fully; a missing one
 * is realpath(parent) + basename.
 */
export async function canonicalFileKey(path: string): Promise<string> {
  const full = resolve(path);
  try {
    return await realpath(full);
  } catch (error) {
    if (!isMissing(error)) throw error;
    return join(await realpathLoose(dirname(full)), basename(full));
  }
}

export function abortError(signal?: AbortSignal): Error {
  if (signal?.reason instanceof Error) return signal.reason;
  const error = new Error("the wait for the lease was aborted");
  error.name = "AbortError";
  return error;
}

/** Notifies the observer at most once per request, however many phases wait. */
export class WaitTracker {
  private last: CoordinationWait | undefined;
  private fired = false;

  constructor(private readonly observer: CoordinatorObserver | undefined) {}

  waiting(wait: CoordinationWait): void {
    this.last = wait;
    if (this.fired) return;
    this.fired = true;
    this.observer?.onWaiting?.(wait);
  }

  /** Holders of the most recent blocked phase, for the busy message. */
  lastHolders(): LeaseHolder[] {
    return this.last?.holders ?? [];
  }

  acquired(): void {
    if (this.fired && this.last) this.observer?.onAcquired?.(this.last);
  }
}

type Mode = "shared" | "exclusive";

interface Entry {
  mode: Mode;
  holder: LeaseHolder;
  release: () => void;
  grant(): void;
  cleanup(): void;
}

/** FIFO reader/writer gate with writer preference; a mutex is exclusive-only. */
class Gate {
  private shared: LeaseHolder[] = [];
  private exclusive: LeaseHolder | undefined;
  private queue: Entry[] = [];

  holders(): LeaseHolder[] {
    return this.exclusive ? [this.exclusive] : [...this.shared];
  }

  idle(): boolean {
    return !this.exclusive && this.shared.length === 0 && !this.queue.length;
  }

  acquire(
    mode: Mode,
    holder: LeaseHolder,
    signal: AbortSignal | undefined,
    onBlock: (holders: LeaseHolder[]) => void,
    onIdle: () => void,
  ): Promise<() => void> {
    if (signal?.aborted) return Promise.reject(abortError(signal));
    return new Promise((resolveLease, reject) => {
      let released = false;
      const entry: Entry = {
        mode,
        holder,
        release: () => {
          if (released) return;
          released = true;
          if (mode === "exclusive") this.exclusive = undefined;
          else this.shared.splice(this.shared.indexOf(holder), 1);
          this.pump();
          if (this.idle()) onIdle();
        },
        grant: () => {
          entry.cleanup();
          resolveLease(entry.release);
        },
        cleanup: () => undefined,
      };
      if (this.queue.length === 0 && this.canGrant(mode)) {
        this.take(entry);
        entry.grant();
        return;
      }
      onBlock(this.holders());
      const onAbort = (): void => {
        const at = this.queue.indexOf(entry);
        if (at >= 0) this.queue.splice(at, 1);
        reject(abortError(signal));
        this.pump();
        if (this.idle()) onIdle();
      };
      signal?.addEventListener("abort", onAbort, { once: true });
      entry.cleanup = () => signal?.removeEventListener("abort", onAbort);
      this.queue.push(entry);
    });
  }

  private canGrant(mode: Mode): boolean {
    return mode === "exclusive"
      ? !this.exclusive && this.shared.length === 0
      : !this.exclusive;
  }

  private take(entry: Entry): void {
    if (entry.mode === "exclusive") this.exclusive = entry.holder;
    else this.shared.push(entry.holder);
  }

  private pump(): void {
    while (this.queue.length > 0) {
      const head = this.queue[0] as Entry;
      if (!this.canGrant(head.mode)) return;
      this.queue.shift();
      this.take(head);
      head.grant();
    }
  }
}

class GateMap {
  private readonly gates = new Map<string, Gate>();

  acquire(
    key: string,
    mode: Mode,
    holder: LeaseHolder,
    signal: AbortSignal | undefined,
    onBlock: (holders: LeaseHolder[]) => void,
  ): Promise<() => void> {
    let gate = this.gates.get(key);
    if (!gate) {
      gate = new Gate();
      this.gates.set(key, gate);
    }
    const owner = gate;
    return owner.acquire(mode, holder, signal, onBlock, () => {
      if (this.gates.get(key) === owner && owner.idle()) this.gates.delete(key);
    });
  }
}

function lease(...releases: Array<() => void>): WorkspaceLease {
  let done = false;
  return {
    async release() {
      if (done) return;
      done = true;
      for (const release of releases) release();
    },
  };
}

function busyError(holders: LeaseHolder[], waitedMs: number): CodingError {
  const who =
    holders.length === 0
      ? "another session holds the workspace"
      : holders
          .map((h) => `another session is running ${h.tool}: ${h.label}`)
          .join("; ");
  return new CodingError(
    "workspace_busy",
    `${who} (waited ${waitedMs}ms). Try again later.`,
  );
}

/**
 * Run an acquisition under an optional wait cap. The cap aborts the wait (not
 * the work after it) and surfaces as `workspace_busy` naming the holders.
 */
export async function withWaitCap<T>(
  input: { signal?: AbortSignal; maxWaitMs?: number },
  tracker: WaitTracker,
  run: (signal: AbortSignal | undefined) => Promise<T>,
): Promise<T> {
  if (input.maxWaitMs === undefined) return run(input.signal);
  const controller = new AbortController();
  const onAbort = (): void => controller.abort(input.signal?.reason);
  if (input.signal?.aborted) controller.abort(input.signal.reason);
  else input.signal?.addEventListener("abort", onAbort, { once: true });
  let busy = false;
  const timer = setTimeout(() => {
    busy = true;
    controller.abort();
  }, input.maxWaitMs);
  try {
    return await run(controller.signal);
  } catch (error) {
    if (busy && !input.signal?.aborted)
      throw busyError(tracker.lastHolders(), input.maxWaitMs);
    throw error;
  } finally {
    clearTimeout(timer);
    input.signal?.removeEventListener("abort", onAbort);
  }
}

export interface InternalAcquire {
  root: string;
  path?: string;
  holder: LeaseHolder;
  signal?: AbortSignal;
  maxWaitMs?: number;
  tracker: WaitTracker;
}

/** In-process coordinator with the tracker exposed so wrappers can share it. */
export class InProcessCoordinator implements WorkspaceCoordinator {
  private readonly workspaces = new GateMap();
  private readonly files = new GateMap();

  constructor(private readonly observer?: CoordinatorObserver) {}

  async acquireFile(input: {
    root: string;
    path: string;
    holder: LeaseHolder;
    signal?: AbortSignal;
    maxWaitMs?: number;
  }): Promise<WorkspaceLease> {
    const tracker = new WaitTracker(this.observer);
    const result = await withWaitCap(input, tracker, (signal) =>
      this.acquireFileTracked({ ...input, signal, tracker }),
    );
    tracker.acquired();
    return result;
  }

  async acquireWorkspace(input: {
    root: string;
    holder: LeaseHolder;
    signal?: AbortSignal;
    maxWaitMs?: number;
  }): Promise<WorkspaceLease> {
    const tracker = new WaitTracker(this.observer);
    const result = await withWaitCap(input, tracker, (signal) =>
      this.acquireWorkspaceTracked({ ...input, signal, tracker }),
    );
    tracker.acquired();
    return result;
  }

  async acquireFileTracked(input: InternalAcquire): Promise<WorkspaceLease> {
    const { holder, signal, tracker } = input;
    const root = await canonicalRootKey(input.root);
    const path = await canonicalFileKey(input.path as string);
    const releaseShared = await this.workspaces.acquire(
      root,
      "shared",
      holder,
      signal,
      (holders) =>
        tracker.waiting({ kind: "workspace", root, waiter: holder, holders }),
    );
    try {
      const releaseFile = await this.files.acquire(
        path,
        "exclusive",
        holder,
        signal,
        (holders) =>
          tracker.waiting({
            kind: "file",
            root,
            path,
            waiter: holder,
            holders,
          }),
      );
      return lease(releaseFile, releaseShared);
    } catch (error) {
      releaseShared();
      throw error;
    }
  }

  async acquireWorkspaceTracked(
    input: InternalAcquire,
  ): Promise<WorkspaceLease> {
    const { holder, signal, tracker } = input;
    const root = await canonicalRootKey(input.root);
    const release = await this.workspaces.acquire(
      root,
      "exclusive",
      holder,
      signal,
      (holders) =>
        tracker.waiting({ kind: "workspace", root, waiter: holder, holders }),
    );
    return lease(release);
  }
}

export function createInProcessWorkspaceCoordinator(
  observer?: CoordinatorObserver,
): WorkspaceCoordinator {
  return new InProcessCoordinator(observer);
}

let shared: InProcessCoordinator | undefined;

/**
 * Shared local gate for durable lock-dir coordinators. Explicit in-process
 * coordinators support file-only coordination, not durable shell ownership.
 */
export function defaultInProcessCoordinator(): InProcessCoordinator {
  shared ??= new InProcessCoordinator();
  return shared;
}

export function defaultWorkspaceCoordinator(): WorkspaceCoordinator {
  return createLockDirWorkspaceCoordinator({
    dir: defaultWorkspaceCoordinationDir(),
  });
}
