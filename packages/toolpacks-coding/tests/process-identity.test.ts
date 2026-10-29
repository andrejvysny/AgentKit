import { describe, expect, test } from "bun:test";
import { discoverExecution } from "../src/process-discovery.js";
import {
  parseDarwinProcessTable,
  parseLinuxProcessStat,
  signalIdentity,
  startedStrictlyBefore,
  type ProcessIdentity,
  type ProcessInspection,
  type ProcessObservation,
} from "../src/process-identity.js";
import { terminate } from "../src/process-teardown.js";

const owned: ProcessIdentity = { pid: 12345, startIdentity: "original" };
const live = (): Promise<ProcessInspection> =>
  Promise.resolve({ state: "same-live" });
const found = async () => ({ identities: [owned], complete: true });
const noWait = { graceMs: 0, survivorWaitMs: 0 };

test("baseline absent ownership scan cannot prove a live process stopped", async () => {
  let scans = 0;
  const signals: string[] = [];
  const result = await terminate({
    ...noWait,
    find: async () => ({
      identities: scans++ === 0 ? [owned] : [],
      complete: true,
    }),
    inspect: live,
    send: (_pid, signal) => signals.push(signal),
  });
  expect(signals).toEqual(["SIGSTOP", "SIGTERM", "SIGCONT", "SIGKILL"]);
  expect(result).toMatchObject({ state: "survivors", survivors: [owned.pid] });
});

test("a reused PID is never signaled after its identity changes", async () => {
  const signals: string[] = [];
  const result = await terminate({
    ...noWait,
    find: found,
    inspect: async () => ({
      state: signals.length === 0 ? "same-live" : "reused",
    }),
    send: (_pid, signal) => signals.push(signal),
  });
  expect(signals).toEqual(["SIGSTOP"]);
  expect(result).toMatchObject({ state: "stopped", survivors: [] });
});

test("lost identity evidence remains unknown, without signaling the numeric PID", async () => {
  let scans = 0;
  const signals: string[] = [];
  const result = await terminate({
    ...noWait,
    find: async () => ({
      identities: scans++ === 0 ? [owned] : [],
      complete: true,
    }),
    inspect: async () => ({ state: scans === 1 ? "same-live" : "unknown" }),
    send: (_pid, signal) => signals.push(signal),
  });
  expect(signals).toEqual(["SIGSTOP"]);
  expect(result).toMatchObject({ state: "unknown", scan: "unsupported" });
});

test.each(["gone", "zombie", "reused"] as const)(
  "explicit %s proof settles the original identity",
  async (state) => {
    const signals: number[] = [];
    const result = await terminate({
      ...noWait,
      find: found,
      inspect: async () => ({ state }),
      send: (pid) => signals.push(pid),
    });
    expect(signals).toEqual([]);
    expect(result.state).toBe("stopped");
  },
);

test("partial discoveries are signaled, but incomplete enumeration stays unknown", async () => {
  const signals: string[] = [];
  let alive = true;
  const result = await terminate({
    ...noWait,
    find: async () => ({ identities: [owned], complete: false }),
    inspect: async () => ({ state: alive ? "same-live" : "gone" }),
    send: (pid, signal) => {
      expect(pid).toBeGreaterThan(0);
      signals.push(signal);
      if (signal === "SIGKILL") alive = false;
    },
  });
  expect(signals).toContain("SIGKILL");
  expect(result).toMatchObject({
    state: "unknown",
    survivors: [],
    scan: "unsupported",
  });
});

test("permission and inspection errors never authorize a send", async () => {
  let sends = 0;
  expect(
    await signalIdentity(
      owned,
      "SIGTERM",
      async () => ({ state: "unknown" }),
      () => {
        sends++;
      },
    ),
  ).toMatchObject({ state: "unknown" });
  expect(
    await signalIdentity(
      owned,
      "SIGTERM",
      async () => {
        throw new Error("EPERM");
      },
      () => {
        sends++;
      },
    ),
  ).toMatchObject({ state: "unknown" });
  expect(sends).toBe(0);
  expect(
    await signalIdentity(owned, "SIGTERM", live, () => {
      throw Object.assign(new Error("denied"), { code: "EPERM" });
    }),
  ).toMatchObject({ state: "unknown" });
});

test("failure persisting final census cannot return stopped", async () => {
  const result = await terminate({
    ...noWait,
    find: async () => ({ identities: [], complete: true }),
    onFinal: async () => {
      throw new Error("disk failed");
    },
  });
  expect(result).toMatchObject({ state: "unknown", scan: "unsupported" });
});

test("group enumeration cannot adopt a reused group's unrelated member", async () => {
  const unrelated: ProcessObservation = {
    pid: owned.pid,
    startIdentity: "replacement",
    ppid: 1,
    pgid: owned.pid,
    state: "live",
  };
  const discovery = await discoverExecution(
    { execId: "execution", rootIdentity: owned },
    {
      marked: async () => ({ processes: [], complete: true }),
      table: async () => ({ processes: [unrelated], complete: true }),
    },
  );
  expect(discovery).toEqual({ identities: [owned], complete: false });
});

test("an owned group anchor allows discovering its current children", async () => {
  const root: ProcessObservation = {
    ...owned,
    ppid: 1,
    pgid: owned.pid,
    state: "live",
  };
  const child: ProcessObservation = {
    pid: 22222,
    startIdentity: "child",
    ppid: owned.pid,
    pgid: owned.pid,
    state: "live",
  };
  const discovery = await discoverExecution(
    { execId: "execution", rootIdentity: owned },
    {
      marked: async () => ({ processes: [], complete: false }),
      table: async () => ({ processes: [root, child], complete: true }),
    },
  );
  expect(discovery.identities).toEqual([
    owned,
    { pid: child.pid, startIdentity: child.startIdentity },
  ]);
  expect(discovery.complete).toBe(false);
});

test("a thrown marker inspection retains successful group discoveries", async () => {
  const root: ProcessObservation = {
    ...owned,
    ppid: 1,
    pgid: owned.pid,
    state: "live",
  };
  const discovery = await discoverExecution(
    { execId: "execution", rootIdentity: owned },
    {
      marked: async () => {
        throw new Error("inspection failed");
      },
      table: async () => ({ processes: [root], complete: true }),
    },
  );
  expect(discovery).toEqual({ identities: [owned], complete: false });
});

test("Darwin parser records start precision, explicit zombie state and malformed evidence", () => {
  const table = parseDarwinProcessTable(
    "123 1 123 Z Wed Oct 28 15:20:01 2026\n456 1 456 S Wed Oct 28 15:20:01 2026 command ENV=x\nbroken\n",
  );
  expect(table.complete).toBe(false);
  expect(table.processes[0]).toMatchObject({
    state: "zombie",
    startIdentity: "ps-lstart:Wed Oct 28 15:20:01 2026",
  });
  expect(table.processes[1]?.command).toBe("command ENV=x");
});

test("Linux identity includes boot and start ticks, despite parentheses in command", () => {
  const fields = ["S", "1", "123", ...Array<string>(16).fill("0"), "987654"];
  expect(
    parseLinuxProcessStat(`123 (worker (name)) ${fields.join(" ")}`, "boot-id"),
  ).toMatchObject({
    pid: 123,
    ppid: 1,
    pgid: 123,
    state: "live",
    startIdentity: "proc-start:boot-id:987654",
  });
});

test("guardian is killed only after durable frozen census, without later CONT", async () => {
  const child = { pid: 22222, startIdentity: "child" };
  const alive = new Set([owned.pid, child.pid]),
    stopped = new Set<number>();
  const signals: { pid: number; signal: string }[] = [];
  let sealed = false;
  const result = await terminate({
    ...noWait,
    guardian: owned,
    initial: [owned, child],
    find: async () => ({ identities: [owned, child], complete: true }),
    inspect: async (identity) => ({
      state: alive.has(identity.pid) ? "same-live" : "gone",
      observation: {
        ...identity,
        ppid: 1,
        pgid: owned.pid,
        state: "live",
        stopped: stopped.has(identity.pid),
      },
    }),
    send: (pid, signal) => {
      signals.push({ pid, signal });
      if (signal === "SIGSTOP") stopped.add(pid);
      if (signal === "SIGTERM") alive.delete(pid);
      if (signal === "SIGKILL") {
        expect(sealed).toBe(true);
        alive.delete(pid);
      }
    },
    onSeal: async (discovery) => {
      expect(discovery).toMatchObject({
        complete: true,
        sealed: true,
        closed: false,
      });
      sealed = true;
    },
  });
  expect(result.state).toBe("stopped");
  expect(
    signals.some(
      ({ pid, signal }) =>
        pid === owned.pid && (signal === "SIGTERM" || signal === "SIGCONT"),
    ),
  ).toBe(false);
  expect(signals.at(-1)).toEqual({ pid: owned.pid, signal: "SIGKILL" });
});

test("failed sealed checkpoint retains frozen guardian without KILL", async () => {
  const signals: string[] = [];
  const result = await terminate({
    ...noWait,
    guardian: owned,
    find: found,
    inspect: async () => ({
      state: "same-live",
      observation: {
        ...owned,
        ppid: 1,
        pgid: owned.pid,
        state: "live",
        stopped: true,
      },
    }),
    send: (_pid, signal) => signals.push(signal),
    onSeal: async () => {
      throw new Error("fsync failed");
    },
  });
  expect(signals).not.toContain("SIGKILL");
  expect(signals).not.toContain("SIGCONT");
  expect(result).toMatchObject({ state: "survivors", scan: "unsupported" });
});

test("start ordering is proven only within one boot and strictly earlier ticks", () => {
  expect(startedStrictlyBefore("proc-start:b:100", "proc-start:b:101")).toBe(
    true,
  );
  expect(startedStrictlyBefore("proc-start:b:101", "proc-start:b:101")).toBe(
    false,
  );
  expect(startedStrictlyBefore("proc-start:b:102", "proc-start:b:101")).toBe(
    false,
  );
  expect(startedStrictlyBefore("proc-start:a:1", "proc-start:b:101")).toBe(
    false,
  );
  expect(startedStrictlyBefore("ps-lstart:x", "ps-lstart:y")).toBe(false);
});

describe("unreadable same-UID processes (non-dumpable environ)", () => {
  const root: ProcessIdentity = {
    pid: 4000,
    startIdentity: "proc-start:b:500",
  };
  const discover = (
    unreadable: ProcessIdentity[],
    rootIdentity?: ProcessIdentity,
  ) =>
    discoverExecution(
      { execId: "execution", ...(rootIdentity ? { rootIdentity } : {}) },
      {
        marked: async () => ({ processes: [], complete: true, unreadable }),
        table: async () => ({ processes: [], complete: true }),
      },
    );

  test("one that started before the execution root cannot carry its marker", async () => {
    const daemon = { pid: 90, startIdentity: "proc-start:b:10" };
    expect((await discover([daemon], root)).complete).toBe(true);
  });

  test("one that started at or after the root stays unproven", async () => {
    for (const ticks of [500, 501])
      expect(
        (
          await discover(
            [{ pid: 4001, startIdentity: `proc-start:b:${ticks}` }],
            root,
          )
        ).complete,
      ).toBe(false);
  });

  test("without a root identity nothing can be excluded", async () => {
    expect(
      (await discover([{ pid: 90, startIdentity: "proc-start:b:10" }]))
        .complete,
    ).toBe(false);
  });

  test("an already-known identity needs no marker proof", async () => {
    expect((await discover([root], root)).complete).toBe(true);
  });
});
