import { execFile } from "node:child_process";
import { readFile, readdir } from "node:fs/promises";
import { promisify } from "node:util";

/** Darwin ps has second precision; Linux uses boot identity plus start ticks. */
export interface ProcessIdentity {
  pid: number;
  startIdentity: string;
}

export interface ProcessObservation extends ProcessIdentity {
  ppid: number;
  pgid: number;
  state: "live" | "zombie";
  stopped?: boolean;
  command?: string;
}

export interface ProcessInspection {
  state: "same-live" | "gone" | "reused" | "zombie" | "unknown";
  observation?: ProcessObservation;
}

export interface ProcessDiscovery {
  identities: ProcessIdentity[];
  complete: boolean;
  sealed?: boolean;
}

export interface ProcessTable {
  processes: ProcessObservation[];
  complete: boolean;
}

const run = promisify(execFile);
let bootIdentity: Promise<string> | undefined;

function errno(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : undefined;
}

/** A failed inspection is never replaced by a successful kill(pid, 0). */
function verifiedMissing(pid: number): "gone" | "unknown" {
  try {
    process.kill(pid, 0);
    return "unknown";
  } catch (error) {
    return errno(error) === "ESRCH" ? "gone" : "unknown";
  }
}

export function parseDarwinProcessTable(text: string): ProcessTable {
  const processes: ProcessObservation[] = [];
  let complete = true;
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const row =
      /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\S+)\s+([A-Z][a-z]{2}\s+[A-Z][a-z]{2}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\d{4})(?:\s+(.*))?$/.exec(
        line,
      );
    if (!row) {
      complete = false;
      continue;
    }
    processes.push({
      pid: Number(row[1]),
      ppid: Number(row[2]),
      pgid: Number(row[3]),
      state: row[4]?.startsWith("Z") ? "zombie" : "live",
      stopped: row[4]?.startsWith("T") ?? false,
      startIdentity: `ps-lstart:${row[5]}`,
      ...(row[6] !== undefined ? { command: row[6] } : {}),
    });
  }
  return { processes, complete };
}

export function parseLinuxProcessStat(
  text: string,
  boot: string,
): ProcessObservation | undefined {
  const head = /^(\d+) \(/.exec(text);
  const end = text.lastIndexOf(") ");
  if (!head || end < 0 || !boot) return undefined;
  const fields = text
    .slice(end + 2)
    .trim()
    .split(/\s+/);
  const [state, ppid, pgid] = fields;
  const started = fields[19];
  if (!state || !ppid || !pgid || !started || !/^\d+$/.test(started))
    return undefined;
  return {
    pid: Number(head[1]),
    ppid: Number(ppid),
    pgid: Number(pgid),
    state: state === "Z" ? "zombie" : "live",
    stopped: state === "T" || state === "t",
    startIdentity: `proc-start:${boot}:${started}`,
  };
}

async function linuxObservation(
  pid: number,
): Promise<ProcessObservation | "gone" | "unknown"> {
  bootIdentity ??= readFile("/proc/sys/kernel/random/boot_id", "utf8").then(
    (value) => value.trim(),
  );
  const boot = await bootIdentity.catch(() => undefined);
  if (!boot) return "unknown";
  try {
    const text = await readFile(`/proc/${pid}/stat`, "utf8");
    return parseLinuxProcessStat(text, boot) ?? "unknown";
  } catch (error) {
    return errno(error) === "ENOENT" || errno(error) === "ESRCH"
      ? "gone"
      : "unknown";
  }
}

async function darwinObservation(
  pid: number,
): Promise<ProcessObservation | "gone" | "unknown"> {
  try {
    const { stdout } = await run(
      "/bin/ps",
      ["-p", String(pid), "-o", "pid=,ppid=,pgid=,stat=,lstart="],
      { env: { ...process.env, LC_ALL: "C" }, timeout: 10_000 },
    );
    const table = parseDarwinProcessTable(stdout);
    return (
      table.processes.find((row) => row.pid === pid) ?? verifiedMissing(pid)
    );
  } catch {
    return verifiedMissing(pid);
  }
}

export async function observeProcess(
  pid: number,
): Promise<ProcessObservation | "gone" | "unknown"> {
  if (!Number.isSafeInteger(pid) || pid <= 0) return "unknown";
  if (process.platform === "linux") return linuxObservation(pid);
  if (process.platform === "darwin") return darwinObservation(pid);
  return "unknown";
}

export async function captureProcessIdentity(
  pid: number,
): Promise<ProcessIdentity | undefined> {
  const observed = await observeProcess(pid);
  return typeof observed === "string"
    ? undefined
    : { pid: observed.pid, startIdentity: observed.startIdentity };
}

export async function inspectProcess(
  identity: ProcessIdentity,
): Promise<ProcessInspection> {
  const observation = await observeProcess(identity.pid);
  if (typeof observation === "string") return { state: observation };
  if (observation.startIdentity !== identity.startIdentity)
    return { state: "reused", observation };
  return {
    state: observation.state === "zombie" ? "zombie" : "same-live",
    observation,
  };
}

export async function readProcessTable(
  environment = false,
): Promise<ProcessTable> {
  if (process.platform === "darwin") {
    try {
      const args = environment
        ? ["-axwwE", "-o", "pid=,ppid=,pgid=,stat=,lstart=,command="]
        : ["-axo", "pid=,ppid=,pgid=,stat=,lstart="];
      const { stdout } = await run("/bin/ps", args, {
        env: { ...process.env, LC_ALL: "C" },
        maxBuffer: 64 * 1024 * 1024,
        timeout: 10_000,
      });
      return parseDarwinProcessTable(stdout);
    } catch {
      return { processes: [], complete: false };
    }
  }
  if (process.platform !== "linux") return { processes: [], complete: false };
  try {
    const rows = await Promise.all(
      (await readdir("/proc"))
        .filter((name) => /^\d+$/.test(name))
        .map((name) => observeProcess(Number(name))),
    );
    return {
      processes: rows.filter(
        (row): row is ProcessObservation => typeof row !== "string",
      ),
      complete: !rows.includes("unknown"),
    };
  } catch {
    return { processes: [], complete: false };
  }
}

export function identityKey(identity: ProcessIdentity): string {
  return `${identity.pid}\0${identity.startIdentity}`;
}

const PROC_START = /^proc-start:(.+):(\d+)$/;

/**
 * True only when `a` provably began before `b`: same boot, strictly fewer
 * start ticks. A shared tick, another boot or the coarse darwin format is
 * never proof.
 */
export function startedStrictlyBefore(a: string, b: string): boolean {
  const left = PROC_START.exec(a);
  const right = PROC_START.exec(b);
  if (!left || !right || left[1] !== right[1]) return false;
  return BigInt(left[2]!) < BigInt(right[2]!);
}

/** POSIX inspection followed by kill is not an atomic kernel identity handle. */
export async function signalIdentity(
  identity: ProcessIdentity,
  signal: NodeJS.Signals,
  inspect: (
    identity: ProcessIdentity,
  ) => Promise<ProcessInspection> = inspectProcess,
  send: (pid: number, signal: NodeJS.Signals) => void = process.kill,
): Promise<ProcessInspection> {
  const checked = await inspect(identity).catch(
    (): ProcessInspection => ({ state: "unknown" }),
  );
  if (checked.state !== "same-live") return checked;
  try {
    send(identity.pid, signal);
    return checked;
  } catch (error) {
    return { state: errno(error) === "ESRCH" ? "gone" : "unknown" };
  }
}
