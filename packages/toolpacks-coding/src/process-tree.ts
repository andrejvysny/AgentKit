/**
 * Find processes by the `AGENTKIT_EXEC_ID` marker every spawned command
 * carries in its environment. The marker is inherited by every descendant,
 * so it finds processes that left the process group (setsid, double fork).
 * A process that scrubs its own environment (`env -i`) escapes it; the group
 * kill is the only net for those.
 */
import { readFile, readdir, stat } from "node:fs/promises";
import {
  identityKey,
  observeProcess,
  readProcessTable,
  type ProcessDiscovery,
  type ProcessIdentity,
} from "./process-identity.js";

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

export interface MarkedIdentity extends MarkedProcess, ProcessIdentity {
  guardian?: boolean;
}
export interface MarkedDiscovery {
  processes: MarkedIdentity[];
  complete: boolean;
  /**
   * Same-UID processes whose environment the kernel refuses to show
   * (non-dumpable: ssh-agent, gpg-agent, CI daemons). Their marker is
   * unproven either way; each consumer decides against its own reference.
   */
  unreadable?: ProcessIdentity[];
}

function isAccessDenied(error: unknown): boolean {
  const code = (error as NodeJS.ErrnoException).code;
  return code === "EACCES" || code === "EPERM";
}

async function linuxMarked(): Promise<MarkedDiscovery> {
  const processes: MarkedIdentity[] = [];
  const unreadable: ProcessIdentity[] = [];
  let complete = true;
  for (const name of await readdir("/proc")) {
    if (!/^\d+$/.test(name) || Number(name) === process.pid) continue;
    try {
      const info = await stat(`/proc/${name}`);
      if (process.getuid && info.uid !== process.getuid()) continue;
      const before = await observeProcess(Number(name));
      if (before === "gone") continue;
      if (before === "unknown") {
        complete = false;
        continue;
      }
      if (before.state === "zombie") continue;
      const raw = await readFile(`/proc/${name}/environ`, "latin1").catch(
        (error: unknown) => {
          if (isAccessDenied(error)) return undefined;
          throw error;
        },
      );
      if (raw === undefined) {
        unreadable.push({
          pid: before.pid,
          startIdentity: before.startIdentity,
        });
        continue;
      }
      const mark = parseEnviron(raw, Number(name));
      if (!mark) continue;
      const command = await readFile(`/proc/${name}/cmdline`, "latin1");
      const after = await observeProcess(mark.pid);
      if (
        typeof after === "string" ||
        after.startIdentity !== before.startIdentity
      ) {
        complete = false;
        continue;
      }
      processes.push({
        ...mark,
        startIdentity: before.startIdentity,
        ...(command.includes("agentkit-command-guardian-v1")
          ? { guardian: true }
          : {}),
      });
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code !== "ENOENT" && code !== "ESRCH") complete = false;
    }
  }
  return { processes, complete, unreadable };
}

/** Partial discoveries are retained when other processes cannot be inspected. */
export async function scanMarkedIdentities(): Promise<MarkedDiscovery> {
  try {
    if (process.platform === "linux") return await linuxMarked();
    if (process.platform !== "darwin")
      return { processes: [], complete: false };
    const table = await readProcessTable(true);
    const processes = table.processes.flatMap((row) => {
      const token = PS_TOKEN.exec(row.command ?? "")?.[1];
      const mark = token ? parseExecId(token) : undefined;
      return mark && row.pid !== process.pid
        ? [
            {
              pid: row.pid,
              startIdentity: row.startIdentity,
              ...mark,
              ...(row.command?.includes("agentkit-command-guardian-v1")
                ? { guardian: true }
                : {}),
            },
          ]
        : [];
    });
    return { processes, complete: table.complete };
  } catch {
    return { processes: [], complete: false };
  }
}

/** Legacy numeric view; teardown uses scanMarkedIdentities instead. */
export async function scanMarked(
  platform: NodeJS.Platform = process.platform,
): Promise<MarkedProcess[] | null> {
  if (platform !== process.platform) return null;
  const result = await scanMarkedIdentities();
  return result.complete
    ? result.processes.map(({ pid, execId, ownerPid }) => ({
        pid,
        execId,
        ownerPid,
      }))
    : null;
}

export async function findMarkedProcesses(
  execId: string,
): Promise<number[] | null> {
  const all = await scanMarked();
  return all
    ? all.filter((mark) => mark.execId === execId).map((mark) => mark.pid)
    : null;
}

export async function processGroupMembers(
  pgid: number,
): Promise<number[] | null> {
  const table = await readProcessTable();
  return table.complete
    ? table.processes
        .filter((row) => row.pgid === pgid && row.state !== "zombie")
        .map((row) => row.pid)
    : null;
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
  private readonly seen = new Map<string, ProcessIdentity>();
  private timer: NodeJS.Timeout | undefined;
  private stopped = false;
  private pending: Promise<unknown> = Promise.resolve();

  constructor(
    private readonly root: ProcessIdentity | undefined,
    private readonly onSample?: (discovery: ProcessDiscovery) => Promise<void>,
    private readonly intervalMs = 100,
    private readonly maxIntervalMs = 1000,
  ) {}

  start(): void {
    let delay = this.intervalMs;
    const tick = (): void => {
      if (this.stopped) return;
      this.pending = this.sample()
        .then((result) => this.onSample?.(result))
        .catch(() => {})
        .finally(() => {
          if (this.stopped) return;
          this.timer = setTimeout(tick, delay);
          delay = Math.min(this.maxIntervalMs, Math.ceil(delay * 1.25));
        });
    };
    tick();
  }

  async stop(): Promise<void> {
    this.stopped = true;
    if (this.timer) clearTimeout(this.timer);
    await this.pending;
  }

  async sample(): Promise<ProcessDiscovery> {
    const table = await readProcessTable();
    const byKey = new Map(
      table.processes.map((row) => [identityKey(row), row]),
    );
    const seeds = [...this.seen.values(), ...(this.root ? [this.root] : [])]
      .filter((identity) => byKey.has(identityKey(identity)))
      .map(({ pid }) => pid);
    const rows = descendantsOf(
      table.processes.map((row) => ({ ...row, started: row.startIdentity })),
      seeds,
    );
    for (const row of rows) {
      if (row.pid !== process.pid) {
        const identity = { pid: row.pid, startIdentity: row.started };
        this.seen.set(identityKey(identity), identity);
      }
    }
    return { identities: [...this.seen.values()], complete: table.complete };
  }
}
