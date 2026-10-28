/**
 * Process-group kill and bounded, tail-keeping command execution.
 *
 * `killProcessTree` adapted from Pi (https://github.com/earendil-works/pi),
 * v1.4.0, commit 4a080252f06caf63a525d7873c5d94d3c23887e0, path
 * `packages/coding-agent/src/utils/shell.ts`; the bounded runner follows
 * `packages/coding-agent/src/core/tools/bash.ts` (timeout, tail truncation,
 * full log in a temp file).
 * Copyright (c) 2025 Mario Zechner. MIT License.
 * Changes: SIGSTOP the group before SIGKILL so it cannot fork during the
 * kill; no Windows branch (macOS-first); streaming accumulator rewritten.
 * `terminate` is new: marker-based descendant discovery, TERM then KILL,
 * survivor report.
 */
import type { ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { type WriteStream, createWriteStream } from "node:fs";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { CodingLimits } from "./limits.js";
import {
  EXEC_ID_ENV,
  scanMarkedIdentities,
  TreeTracker,
} from "./process-tree.js";
import {
  captureProcessIdentity,
  observeProcess,
  type ProcessDiscovery,
  type ProcessIdentity,
} from "./process-identity.js";
import { discoverExecution } from "./process-discovery.js";
import {
  GUARDIAN_PROTOCOL,
  launchGuardian,
  type GuardianLaunch,
} from "./process-launcher.js";
import { terminate, type Teardown } from "./process-teardown.js";
export { terminate, type Teardown } from "./process-teardown.js";
import { formatSize, truncateTail } from "./truncate.js";

/** Compatibility helper; a numeric PID alone never authorizes signaling a group. */
export async function killProcessTree(pid: number): Promise<void> {
  const identity = await captureProcessIdentity(pid);
  if (!identity) return;
  await terminate({
    initial: [identity],
    find: () => discoverExecution({ execId: "", rootIdentity: identity }),
    graceMs: 0,
  });
}

export interface RunSpec {
  file: string;
  args: string[];
  cwd: string;
  env: NodeJS.ProcessEnv;
  timeoutMs: number;
  signal?: AbortSignal | undefined;
  scratch: string;
  logName: string;
  limits: CodingLimits;
  /** Supplied by durable command ownership; generated here for direct callers. */
  execId?: string;
  onSpawn?: (
    pid: number,
    identity?: ProcessIdentity,
    guardianProtocol?: typeof GUARDIAN_PROTOCOL,
  ) => Promise<void>;
  onDiscovery?: (
    discovery: ProcessDiscovery & { closed?: boolean },
  ) => Promise<void>;
}

export interface RunResult {
  /** Tail of the output, already truncated to the limits. */
  output: string;
  /** True when `output` is not the whole output. */
  truncated: boolean;
  notice: string;
  logPath?: string | undefined;
  fullLogTruncated: boolean;
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  timedOut: boolean;
  aborted: boolean;
  spawnError?: string | undefined;
  teardown: Teardown;
}

const KILL_GRACE_CAP_MS = 10_000;

/**
 * Kill commands left behind by agent processes that no longer exist (a crash
 * skips every teardown). Best effort, never throws; returns how many marked
 * processes it found. Call once at boot. A recycled owner pid hides its
 * orphans until that pid is free again.
 */
export async function sweepOrphans(
  options: { graceMs?: number } = {},
): Promise<number> {
  return (await sweepOrphansDetailed(options)).seen;
}

/** Await this verdict before admitting mutations; inspection failure is unknown. */
export async function sweepOrphansDetailed(
  options: { graceMs?: number } = {},
): Promise<Teardown & { seen: number }> {
  try {
    const find = async (): Promise<ProcessDiscovery> => {
      const marked = await scanMarkedIdentities();
      const identities: ProcessIdentity[] = [];
      let complete = marked.complete;
      // Durable workspace reconciliation owns guardian groups; sweeping their
      // leader first would destroy the identity anchor needed for recovery.
      const guarded = new Set(
        marked.processes.filter((row) => row.guardian).map((row) => row.execId),
      );
      for (const row of marked.processes) {
        if (guarded.has(row.execId)) continue;
        const owner = await observeProcess(row.ownerPid);
        if (owner === "unknown") complete = false;
        if (
          owner === "gone" ||
          (typeof owner !== "string" && owner.state === "zombie")
        )
          identities.push({ pid: row.pid, startIdentity: row.startIdentity });
      }
      return { identities, complete };
    };
    return await terminate({
      find,
      graceMs: Math.min(options.graceMs ?? 2000, KILL_GRACE_CAP_MS),
    });
  } catch {
    return {
      state: "unknown",
      escalated: false,
      survivors: [],
      scan: "unsupported",
      seen: 0,
    };
  }
}

/** Keeps a bounded tail in memory; spills the full stream to a scratch file. */
class Tail {
  private chunks: Buffer[] = [];
  private held = 0;
  private total = 0;
  private spill: WriteStream | undefined;
  private spilled = 0;
  private logFailed = false;
  private dropped = false;
  logPath: string | undefined;

  get fullLogTruncated(): boolean {
    return (
      this.logPath !== undefined &&
      (this.logFailed || this.spilled < this.total)
    );
  }

  recordStoredBytes(bytes: number): void {
    this.spilled = bytes;
  }

  constructor(
    private readonly limits: CodingLimits,
    private readonly scratch: string,
    private readonly logName: string,
  ) {}

  push(chunk: Buffer): void {
    this.total += chunk.length;
    const hold = this.limits.commandTailMaxBytes * 2;
    if (!this.spill && this.total > hold) this.startSpill();
    if (this.spill) this.writeSpill(chunk);
    this.chunks.push(chunk);
    this.held += chunk.length;
    while (this.held - (this.chunks[0]?.length ?? 0) >= hold) {
      this.held -= (this.chunks.shift() as Buffer).length;
      this.dropped = true;
    }
  }

  private startSpill(): void {
    this.logPath = join(this.scratch, this.logName);
    this.spill = createWriteStream(this.logPath, { flags: "w", mode: 0o600 });
    this.spill.on("error", () => {
      this.logFailed = true;
      this.spill = undefined;
    });
    for (const chunk of this.chunks) this.writeSpill(chunk);
  }

  private writeSpill(chunk: Buffer): void {
    if (!this.spill) return;
    const room = this.limits.commandMaxLogBytes - this.spilled;
    if (room <= 0) return;
    const part = chunk.length > room ? chunk.subarray(0, room) : chunk;
    this.spilled += part.length;
    this.spill.write(part);
  }

  async finish(): Promise<{
    text: string;
    total: number;
    dropped: boolean;
    buffer: Buffer;
  }> {
    const stream = this.spill;
    if (stream) await new Promise<void>((done) => stream.end(() => done()));
    const buffer = Buffer.concat(this.chunks);
    const text = buffer.toString("utf-8").replace(/^�+/, "");
    return { text, total: this.total, dropped: this.dropped, buffer };
  }
}

function describeTruncation(
  total: number,
  shownLines: number,
  shownBytes: number,
  logPath: string | undefined,
): string {
  const where = logPath
    ? ` Full output (scratch, outside the workspace): ${logPath}`
    : "";
  return `[Output truncated: showing the last ${shownLines} lines (${formatSize(shownBytes)} of ${formatSize(total)}).${where}]`;
}

/** Run one process group to completion, timeout or abort. */
export async function runBounded(spec: RunSpec): Promise<RunResult> {
  const tail = new Tail(spec.limits, spec.scratch, spec.logName);
  let child: ChildProcess;
  let launch: GuardianLaunch;
  // Inherited by every descendant, which is how teardown and the crash sweep
  // find the ones that left the process group.
  if (spec.signal?.aborted)
    return failedSpawn(new Error("command aborted before spawn"));
  const execId = spec.execId ?? `${process.pid}:${randomUUID()}`;
  try {
    launch = launchGuardian(spec.scratch, execId, {
      file: spec.file,
      args: spec.args,
      cwd: spec.cwd,
      env: { ...spec.env, [EXEC_ID_ENV]: execId },
    });
    child = launch.child;
  } catch (error) {
    return failedSpawn(error);
  }
  let timedOut = false;
  let aborted = false;
  let exitCode: number | null = null;
  let exitSignal: NodeJS.Signals | null = null;
  let spawnError: string | undefined;
  let commandReported = false;
  const rootIdentity =
    child.pid === undefined
      ? Promise.resolve(undefined)
      : captureProcessIdentity(child.pid);
  let tracker: TreeTracker | undefined;
  let censusSealed = false;
  const find = async (): Promise<ProcessDiscovery> => {
    const tracked = tracker
      ? await tracker.sample()
      : { identities: [], complete: true };
    const root = await rootIdentity;
    const discovered = await discoverExecution({
      execId,
      ...(root ? { rootIdentity: root } : {}),
      ...(child.pid !== undefined ? { rootPid: child.pid } : {}),
      known: tracked.identities,
    });
    const census = {
      identities: discovered.identities,
      complete: tracked.complete && discovered.complete,
    };
    if (!censusSealed)
      await spec.onDiscovery?.({
        ...census,
        closed: false,
        sealed: false,
      });
    return census;
  };
  let teardownRun: Promise<Teardown> | undefined;
  let finishExecution: (() => void) | undefined;
  const tearDown = (): Promise<Teardown> => {
    teardownRun ??= (async () => {
      await tracker?.stop();
      const root = await rootIdentity;
      const { state, escalated, survivors, scan } = await terminate({
        ...(root ? { initial: [root] } : {}),
        ...(root ? { guardian: root } : {}),
        find,
        graceMs: Math.min(spec.limits.commandKillGraceMs, KILL_GRACE_CAP_MS),
        ...(spec.onDiscovery ? { onFinal: spec.onDiscovery } : {}),
        onSeal: async (discovery) => {
          await spec.onDiscovery?.(discovery);
          censusSealed = true;
        },
      });
      return { state, escalated, survivors, scan };
    })();
    return teardownRun;
  };
  const timer = setTimeout(() => {
    timedOut = true;
    void tearDown().finally(() => {
      child.stdout?.destroy();
      child.stderr?.destroy();
      finishExecution?.();
    });
  }, spec.timeoutMs);
  const onAbort = () => {
    aborted = true;
    void tearDown().finally(() => {
      child.stdout?.destroy();
      child.stderr?.destroy();
      finishExecution?.();
    });
  };
  if (spec.signal?.aborted) onAbort();
  else spec.signal?.addEventListener("abort", onAbort, { once: true });

  const finished = new Promise<void>((resolveRun) => {
    let settled = false;
    let grace: NodeJS.Timeout | undefined;
    const finish = () => {
      if (settled) return;
      settled = true;
      if (grace) clearTimeout(grace);
      resolveRun();
    };
    finishExecution = finish;
    child.once("error", (error) => {
      spawnError = error.message;
      finish();
    });
    // Background children can keep the pipes open after the shell exits.
    // Wait until the pipes have been quiet for a moment (so a loaded machine
    // can still drain them), then sweep the group and stop waiting.
    let exited = false;
    const armGrace = () => {
      if (grace) clearTimeout(grace);
      grace = setTimeout(
        () => {
          void tearDown();
          child.stdout?.destroy();
          child.stderr?.destroy();
          finish();
        },
        timedOut || aborted ? spec.limits.commandKillGraceMs : 250,
      );
    };
    const onData = (chunk: Buffer) => {
      tail.push(chunk);
      if (exited && !settled) armGrace();
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.once("exit", (code, sig) => {
      if (!commandReported) {
        exitCode = code;
        exitSignal = sig;
      }
      exited = true;
      armGrace();
    });
    child.once("close", finish);
  });
  void launch.result.then(async (result) => {
    commandReported = true;
    exitCode = result.exitCode;
    exitSignal = result.signal;
    spawnError = result.spawnError;
    await tearDown();
    child.stdout?.destroy();
    child.stderr?.destroy();
    finishExecution?.();
  });
  if (child.pid !== undefined) {
    try {
      const identity = await rootIdentity;
      if (!identity)
        throw new Error("command identity unavailable; execution refused");
      await spec.onSpawn?.(child.pid, identity, GUARDIAN_PROTOCOL);
      if (!teardownRun && !spec.signal?.aborted && !timedOut) {
        tracker = new TreeTracker(identity, spec.onDiscovery);
        tracker.start();
        launch.release();
      } else await tearDown();
    } catch (error) {
      spawnError = error instanceof Error ? error.message : String(error);
      await tearDown();
      launch.dispose();
      child.stdout?.destroy();
      child.stderr?.destroy();
      finishExecution?.();
    }
  }
  await finished;
  clearTimeout(timer);
  spec.signal?.removeEventListener("abort", onAbort);
  // Sweep stragglers left by a command that exited with children alive.
  const teardown = await tearDown();
  launch.dispose();
  if (teardown.state !== "stopped") child.unref();

  const collected = await tail.finish();
  const cut = truncateTail(collected.text, {
    maxLines: spec.limits.commandTailMaxLines,
    maxBytes: spec.limits.commandTailMaxBytes,
  });
  const keptAll = !collected.dropped && !cut.truncated;
  if (!keptAll && !tail.logPath && !collected.dropped) {
    // Everything is still in memory: the model gets the full log's path too.
    tail.logPath = join(spec.scratch, spec.logName);
    const stored = collected.buffer.subarray(0, spec.limits.commandMaxLogBytes);
    await writeFile(tail.logPath, stored, { mode: 0o600 });
    tail.recordStoredBytes(stored.length);
  }
  const result: RunResult = {
    output: cut.content,
    truncated: !keptAll,
    notice: keptAll
      ? ""
      : describeTruncation(
          collected.total,
          cut.outputLines,
          cut.outputBytes,
          tail.logPath,
        ),
    logPath: tail.logPath,
    fullLogTruncated: tail.fullLogTruncated,
    exitCode,
    signal: exitSignal,
    timedOut,
    aborted,
    spawnError,
    teardown,
  };
  return result;

  function failedSpawn(error: unknown): RunResult {
    return {
      output: "",
      truncated: false,
      fullLogTruncated: false,
      notice: "",
      exitCode: null,
      signal: null,
      timedOut: false,
      aborted: spec.signal?.aborted ?? false,
      spawnError: error instanceof Error ? error.message : String(error),
      teardown: {
        state: "stopped",
        escalated: false,
        survivors: [],
        scan: "ok",
      },
    };
  }
}
