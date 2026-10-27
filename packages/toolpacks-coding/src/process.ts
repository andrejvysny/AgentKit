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
import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { type WriteStream, createWriteStream } from "node:fs";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { CodingLimits } from "./limits.js";
import {
  EXEC_ID_ENV,
  findMarkedProcesses,
  scanMarked,
  TreeTracker,
} from "./process-tree.js";
import { formatSize, truncateTail } from "./truncate.js";

/** Kill `pid` and every process in its group. */
export function killProcessTree(pid: number): void {
  try {
    process.kill(-pid, "SIGSTOP");
  } catch {
    // group may be gone, or never was one
  }
  try {
    process.kill(-pid, "SIGKILL");
  } catch {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      // already dead
    }
  }
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
}

export interface RunResult {
  /** Tail of the output, already truncated to the limits. */
  output: string;
  /** True when `output` is not the whole output. */
  truncated: boolean;
  notice: string;
  logPath?: string | undefined;
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  timedOut: boolean;
  aborted: boolean;
  spawnError?: string | undefined;
  teardown: Teardown;
}

/** What the teardown of one command found and had to do. */
export interface Teardown {
  /** True when SIGTERM was not enough and SIGKILL was sent. */
  escalated: boolean;
  /** Marked pids still alive after SIGKILL. */
  survivors: number[];
  /** "unsupported": descendants outside the group could not be looked for. */
  scan: "ok" | "unsupported";
}

const KILL_GRACE_CAP_MS = 10_000;
const SURVIVOR_WAIT_MS = 500;
const POLL_MS = 25;

function signalPid(pid: number, sig: NodeJS.Signals): void {
  try {
    process.kill(pid, sig);
  } catch {
    // already gone
  }
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

const sleep = (ms: number) => new Promise<void>((done) => setTimeout(done, ms));

interface TerminateSpec {
  /** Process group to signal; undefined when only marked pids are known. */
  pgid?: number | undefined;
  find: () => Promise<number[] | null>;
  graceMs: number;
}

/**
 * SIGTERM the group and every marked pid, wait `graceMs`, SIGKILL what is
 * left, then report what still lives. `seen` counts distinct marked pids.
 */
async function terminate(
  spec: TerminateSpec,
): Promise<Teardown & { seen: number }> {
  const known = new Set<number>();
  let scan: Teardown["scan"] = "ok";
  const refresh = async () => {
    const pids = await spec.find();
    if (pids === null) scan = "unsupported";
    else for (const pid of pids) known.add(pid);
  };
  const signalAll = (sig: NodeJS.Signals) => {
    if (spec.pgid !== undefined) signalPid(-spec.pgid, sig);
    for (const pid of known) signalPid(pid, sig);
  };
  const remaining = () =>
    (spec.pgid !== undefined && pidAlive(-spec.pgid)) ||
    [...known].some(pidAlive);

  await refresh();
  if (known.size === 0 && !remaining())
    return { escalated: false, survivors: [], scan, seen: 0 };
  // Freeze first so nothing can fork between discovery and the signal; rescan
  // until the set stops growing. A stopped process only sees SIGTERM once
  // continued, hence the SIGCONT.
  for (let i = 0; i < 3; i++) {
    signalAll("SIGSTOP");
    const before = known.size;
    await refresh();
    if (known.size === before) break;
  }
  signalAll("SIGTERM");
  signalAll("SIGCONT");
  const deadline = Date.now() + spec.graceMs;
  while (remaining() && Date.now() < deadline) await sleep(POLL_MS);
  const escalated = remaining();
  if (escalated) {
    await refresh();
    signalAll("SIGKILL");
    const end = Date.now() + SURVIVOR_WAIT_MS;
    while (remaining() && Date.now() < end) await sleep(POLL_MS);
  }
  let survivors = [...known].filter(pidAlive);
  if (survivors.length > 0 && scan === "ok") {
    // A zombie nobody reaped still answers kill(pid, 0) but has no
    // environment to find; only a process the scan still sees is a survivor.
    const live = await spec.find();
    if (live) survivors = survivors.filter((pid) => live.includes(pid));
  }
  return { escalated, survivors, scan, seen: known.size };
}

/**
 * Kill commands left behind by agent processes that no longer exist (a crash
 * skips every teardown). Best effort, never throws; returns how many marked
 * processes it found. Call once at boot. A recycled owner pid hides its
 * orphans until that pid is free again.
 */
export async function sweepOrphans(
  options: { graceMs?: number } = {},
): Promise<number> {
  try {
    const find = async () => {
      const all = await scanMarked();
      return all
        ? all.filter((m) => !pidAlive(m.ownerPid)).map((m) => m.pid)
        : null;
    };
    const { seen } = await terminate({
      find,
      graceMs: Math.min(options.graceMs ?? 2000, KILL_GRACE_CAP_MS),
    });
    return seen;
  } catch {
    return 0;
  }
}

/** Keeps a bounded tail in memory; spills the full stream to a scratch file. */
class Tail {
  private chunks: Buffer[] = [];
  private held = 0;
  private total = 0;
  private spill: WriteStream | undefined;
  private spilled = 0;
  private dropped = false;
  logPath: string | undefined;

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
  // Inherited by every descendant, which is how teardown and the crash sweep
  // find the ones that left the process group.
  const execId = `${process.pid}:${randomUUID()}`;
  try {
    child = spawn(spec.file, spec.args, {
      cwd: spec.cwd,
      env: { ...spec.env, [EXEC_ID_ENV]: execId },
      detached: true,
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (error) {
    return failedSpawn(error);
  }
  let timedOut = false;
  let aborted = false;
  let exitCode: number | null = null;
  let exitSignal: NodeJS.Signals | null = null;
  let spawnError: string | undefined;
  // The marker misses Apple-signed binaries on macOS, so sample the tree too.
  const tracker =
    process.platform === "darwin" && child.pid !== undefined
      ? new TreeTracker(child.pid)
      : undefined;
  tracker?.start();
  const find = async (): Promise<number[] | null> => {
    const [marked, tracked] = await Promise.all([
      findMarkedProcesses(execId),
      tracker ? tracker.sample() : Promise.resolve([]),
    ]);
    if (marked === null && tracked === null) return null;
    return [...new Set([...(marked ?? []), ...(tracked ?? [])])];
  };
  let teardownRun: Promise<Teardown> | undefined;
  const tearDown = (): Promise<Teardown> => {
    teardownRun ??= terminate({
      pgid: child.pid,
      find,
      graceMs: Math.min(spec.limits.commandKillGraceMs, KILL_GRACE_CAP_MS),
    })
      .then(({ escalated, survivors, scan }) => ({
        escalated,
        survivors,
        scan,
      }))
      .finally(() => tracker?.stop());
    return teardownRun;
  };
  const timer = setTimeout(() => {
    timedOut = true;
    void tearDown();
  }, spec.timeoutMs);
  const onAbort = () => {
    aborted = true;
    void tearDown();
  };
  if (spec.signal?.aborted) onAbort();
  else spec.signal?.addEventListener("abort", onAbort, { once: true });

  await new Promise<void>((resolveRun) => {
    let settled = false;
    let grace: NodeJS.Timeout | undefined;
    const finish = () => {
      if (settled) return;
      settled = true;
      if (grace) clearTimeout(grace);
      resolveRun();
    };
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
      exitCode = code;
      exitSignal = sig;
      exited = true;
      armGrace();
    });
    child.once("close", finish);
  });
  clearTimeout(timer);
  spec.signal?.removeEventListener("abort", onAbort);
  // Sweep stragglers left by a command that exited with children alive.
  const teardown = await tearDown();

  const collected = await tail.finish();
  const cut = truncateTail(collected.text, {
    maxLines: spec.limits.commandTailMaxLines,
    maxBytes: spec.limits.commandTailMaxBytes,
  });
  const keptAll = !collected.dropped && !cut.truncated;
  if (!keptAll && !tail.logPath && !collected.dropped) {
    // Everything is still in memory: the model gets the full log's path too.
    tail.logPath = join(spec.scratch, spec.logName);
    await writeFile(tail.logPath, collected.buffer, { mode: 0o600 });
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
      notice: "",
      exitCode: null,
      signal: null,
      timedOut: false,
      aborted: false,
      spawnError: error instanceof Error ? error.message : String(error),
      teardown: { escalated: false, survivors: [], scan: "ok" },
    };
  }
}
