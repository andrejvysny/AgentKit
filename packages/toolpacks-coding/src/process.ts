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
 */
import { type ChildProcess, spawn } from "node:child_process";
import { type WriteStream, createWriteStream } from "node:fs";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { CodingLimits } from "./limits.js";
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
  try {
    child = spawn(spec.file, spec.args, {
      cwd: spec.cwd,
      env: spec.env,
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
  const kill = () => {
    if (child.pid !== undefined) killProcessTree(child.pid);
  };
  const timer = setTimeout(() => {
    timedOut = true;
    kill();
  }, spec.timeoutMs);
  const onAbort = () => {
    aborted = true;
    kill();
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
          kill();
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
  kill();

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
    };
  }
}
