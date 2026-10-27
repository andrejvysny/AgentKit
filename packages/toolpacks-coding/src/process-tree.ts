/**
 * Find processes by the `AGENTKIT_EXEC_ID` marker every spawned command
 * carries in its environment. The marker is inherited by every descendant,
 * so it finds processes that left the process group (setsid, double fork).
 * A process that scrubs its own environment (`env -i`) escapes it; the group
 * kill is the only net for those.
 */
import { execFile } from "node:child_process";
import { readFile, readdir } from "node:fs/promises";

export const EXEC_ID_ENV = "AGENTKIT_EXEC_ID";

export interface MarkedProcess {
  pid: number;
  execId: string;
  /** pid of the agent process that spawned the command. */
  ownerPid: number;
}

const ID_PATTERN = /^(\d+):(\S+)$/;
// `ps -E` appends the environment to the command line, space separated.
const PS_TOKEN = new RegExp(`(?:^|\\s)${EXEC_ID_ENV}=(\\d+:\\S+)(?=\\s|$)`);

/** `"<ownerPid>:<token>"` to its owner pid; undefined when malformed. */
export function parseExecId(
  value: string,
): { ownerPid: number; execId: string } | undefined {
  const match = ID_PATTERN.exec(value);
  if (!match) return undefined;
  const ownerPid = Number(match[1]);
  if (!Number.isSafeInteger(ownerPid) || ownerPid <= 0) return undefined;
  return { ownerPid, execId: value };
}

/** Parse `ps -axwwE -o pid=,command=` output. */
export function parsePsOutput(text: string): MarkedProcess[] {
  const found: MarkedProcess[] = [];
  for (const line of text.split("\n")) {
    const head = /^\s*(\d+)\s(.*)$/.exec(line);
    if (!head) continue;
    const token = PS_TOKEN.exec(head[2] ?? "")?.[1];
    const parsed = token ? parseExecId(token) : undefined;
    if (parsed) found.push({ pid: Number(head[1]), ...parsed });
  }
  return found;
}

/** Parse the raw contents of `/proc/<pid>/environ` (NUL separated). */
export function parseEnviron(
  raw: string,
  pid: number,
): MarkedProcess | undefined {
  const prefix = `${EXEC_ID_ENV}=`;
  for (const entry of raw.split("\0")) {
    if (!entry.startsWith(prefix)) continue;
    const parsed = parseExecId(entry.slice(prefix.length));
    return parsed ? { pid, ...parsed } : undefined;
  }
  return undefined;
}

function run(file: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(
      file,
      args,
      { maxBuffer: 64 * 1024 * 1024, timeout: 10_000 },
      (error, stdout) => (error ? reject(error) : resolve(stdout)),
    );
  });
}

async function scanProc(): Promise<MarkedProcess[]> {
  const found: MarkedProcess[] = [];
  for (const name of await readdir("/proc")) {
    if (!/^\d+$/.test(name)) continue;
    const raw = await readFile(`/proc/${name}/environ`, "latin1").catch(
      () => "",
    );
    const mark = parseEnviron(raw, Number(name));
    if (mark) found.push(mark);
  }
  return found;
}

/**
 * Every marked process except this one; null when the platform cannot be
 * scanned or the scan failed. Never throws.
 */
export async function scanMarked(
  platform: NodeJS.Platform = process.platform,
): Promise<MarkedProcess[] | null> {
  try {
    let all: MarkedProcess[];
    if (platform === "darwin")
      all = parsePsOutput(
        await run("/bin/ps", ["-axwwE", "-o", "pid=,command="]),
      );
    else if (platform === "linux") all = await scanProc();
    else return null;
    return all.filter((mark) => mark.pid !== process.pid);
  } catch {
    return null;
  }
}

/** Pids carrying exactly this exec id; null when unsupported. */
export async function findMarkedProcesses(
  execId: string,
): Promise<number[] | null> {
  const all = await scanMarked();
  return all ? all.filter((m) => m.execId === execId).map((m) => m.pid) : null;
}

export interface PsRow {
  pid: number;
  ppid: number;
  /** `lstart`, which tells a recycled pid from the original process. */
  started: string;
}

/** Parse `ps -axo pid=,ppid=,lstart=` output. */
export function parsePsTable(text: string): PsRow[] {
  const rows: PsRow[] = [];
  for (const line of text.split("\n")) {
    const m = /^\s*(\d+)\s+(\d+)\s+(\S.*\S)\s*$/.exec(line);
    if (m)
      rows.push({
        pid: Number(m[1]),
        ppid: Number(m[2]),
        started: m[3] as string,
      });
  }
  return rows;
}

/** Rows at or below any seed pid, following parent links. */
export function descendantsOf(
  rows: readonly PsRow[],
  seeds: Iterable<number>,
): PsRow[] {
  const children = new Map<number, PsRow[]>();
  for (const row of rows) {
    const list = children.get(row.ppid) ?? [];
    list.push(row);
    children.set(row.ppid, list);
  }
  const byPid = new Map(rows.map((row) => [row.pid, row]));
  const out = new Map<number, PsRow>();
  const queue = [...seeds];
  while (queue.length > 0) {
    const pid = queue.pop() as number;
    const self = byPid.get(pid);
    if (self) out.set(pid, self);
    for (const child of children.get(pid) ?? [])
      if (!out.has(child.pid)) {
        out.set(child.pid, child);
        queue.push(child.pid);
      }
  }
  return [...out.values()];
}

/**
 * macOS hides the environment of Apple-signed binaries (bash, sleep, perl),
 * so the marker cannot find what a shell script leaves behind. Instead the
 * process table is sampled while the command runs and every descendant of
 * the root is remembered; a double fork that reparents to launchd stays
 * known, unless it is created and orphaned between two samples.
 */
export class TreeTracker {
  private readonly seen = new Map<number, string>();
  private timer: NodeJS.Timeout | undefined;
  private stopped = false;
  private rootGone = false;

  constructor(
    private readonly rootPid: number,
    private readonly intervalMs = 100,
    private readonly maxIntervalMs = 1000,
  ) {}

  /**
   * Fast while a command is young (that is when shells fork and detach), then
   * backing off: one `ps` costs tens of ms, and a 10-minute build sampled at
   * 100 ms would spend a large share of a core on watching itself.
   */
  start(): void {
    let delay = this.intervalMs;
    const tick = () => {
      if (this.stopped) return;
      void this.sample().finally(() => {
        if (this.stopped) return;
        this.timer = setTimeout(tick, delay);
        delay = Math.min(this.maxIntervalMs, Math.ceil(delay * 1.25));
      });
    };
    tick();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer) clearTimeout(this.timer);
  }

  /** Sample now; the tracked pids that are still the same live processes. */
  async sample(): Promise<number[] | null> {
    let rows: PsRow[];
    try {
      rows = parsePsTable(await run("/bin/ps", ["-axo", "pid=,ppid=,lstart="]));
    } catch {
      return null;
    }
    const byPid = new Map(rows.map((row) => [row.pid, row]));
    for (const [pid, started] of this.seen)
      if (byPid.get(pid)?.started !== started) this.seen.delete(pid);
    if (!byPid.has(this.rootPid)) this.rootGone = true;
    const seeds = this.rootGone ? [] : [this.rootPid];
    for (const row of descendantsOf(rows, [...seeds, ...this.seen.keys()]))
      if (row.pid !== process.pid) this.seen.set(row.pid, row.started);
    return [...this.seen.keys()];
  }
}
