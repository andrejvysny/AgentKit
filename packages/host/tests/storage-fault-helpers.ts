/**
 * Shared plumbing for the M0 storage fault suites
 * (`fault-storage-unavailable.test.ts`, `fault-storage-full.test.ts`).
 *
 * The pattern is the one `claim-failure-injection.test.ts` set in
 * adapters-sqlite: a rig that behaves exactly like the real thing until it is
 * ARMED, then fails named operations with a named storage error. The
 * difference is the seam. The claim suite faults the clock and the id
 * generator — the two seams a claim crosses below the port. These suites fault
 * the PORT itself: every method of every port of an `AssistantStore` (and of
 * the transaction view a `transaction()` callback is handed, which is where
 * the durable commits actually happen) can be made to fail with an
 * `ENOSPC`/`EIO`/`SQLITE_CANTOPEN`-class error, which is exactly what the
 * sqlite adapter raises when the device is full or the file is gone. Faulting
 * at this seam is the host-side equivalent of pulling the disk: the host must
 * react correctly no matter which write the storage layer died on.
 *
 * Deliberate properties:
 * - PERSISTENT until cleared: a real full disk does not intermittently accept
 *   writes; once a fault is armed, every call past the `after` budget throws.
 * - COUNTED before throwing: suites probe a clean run's call counts and then
 *   fault "the last durable write" precisely, like the injection-point table
 *   of the claim suite faults the nth id generation.
 * - TRANSPARENT until armed: identity-preserving pass-through, so seeding and
 *   post-recovery reads behave against the untouched adapter.
 */
import type { AssistantStore } from "../src/index.js";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Storage-class error codes the suites inject, mirroring sqlite/OS errno. */
export type StorageFaultCode =
  | "ENOSPC"
  | "EIO"
  | "SQLITE_CANTOPEN"
  | "SQLITE_IOERR";

/** The error a faulted port method throws; carries the code a client switches on. */
export class StorageFaultError extends Error {
  readonly code: StorageFaultCode;
  readonly storageFault = true;
  constructor(code: StorageFaultCode, message: string) {
    super(message);
    this.name = "StorageFaultError";
    this.code = code;
  }
}

export interface FaultSpec {
  /** Number of calls to let through before the fault starts biting. */
  after?: number;
  code?: StorageFaultCode;
  message?: string;
}

interface ResolvedFault {
  after: number;
  code: StorageFaultCode;
  message: string;
}

const DEFAULT_MESSAGES: Record<StorageFaultCode, string> = {
  ENOSPC:
    "attempt to write a database file (disk I/O error): no space left on device",
  EIO: "disk I/O error",
  SQLITE_CANTOPEN: "unable to open database file",
  SQLITE_IOERR: "disk I/O error",
};

export interface StorageFaultRig {
  /** The store to hand the host; identical to `raw` while unarmed. */
  readonly faulty: AssistantStore;
  /** The untouched adapter; the post-recovery reads go through this. */
  readonly raw: AssistantStore;
  /** Fail `port.method` (e.g. `tasks.createTask`) persistently. */
  arm(key: string, spec?: FaultSpec): void;
  /** Fail every method whose `port.method` matches — whole-store loss. */
  armPattern(pattern: RegExp, spec?: FaultSpec): void;
  /** Forget every armed fault; the device is back. */
  clear(): void;
  /** How many times `key` (or a matching method, given a RegExp) has run. */
  calls(key: string | RegExp): number;
  /** The keys that currently throw. */
  armed(): string[];
}

/**
 * `store.transaction` is the key for the aggregate-level `transaction()`
 * entry point — arming it simulates a device so full that `BEGIN IMMEDIATE`
 * cannot even create the journal file.
 */
export const TRANSACTION_KEY = "store.transaction";

export function createStorageFaultRig(raw: AssistantStore): StorageFaultRig {
  const exact = new Map<string, ResolvedFault>();
  const patterns: { pattern: RegExp; fault: ResolvedFault }[] = [];
  const counts = new Map<string, number>();
  const portProxies = new WeakMap<object, unknown>();

  const resolve = (key: string): ResolvedFault | undefined => {
    const hit = exact.get(key);
    if (hit) return hit;
    for (const entry of patterns)
      if (entry.pattern.test(key)) return entry.fault;
    return undefined;
  };

  const methodWrapper =
    (
      portLabel: string,
      method: string,
      impl: (...args: unknown[]) => unknown,
      receiver: object,
    ) =>
    async (...args: unknown[]) => {
      const key = `${portLabel}.${method}`;
      const fault = resolve(key);
      const seen = (counts.get(key) ?? 0) + 1;
      counts.set(key, seen);
      if (fault && seen > fault.after)
        throw new StorageFaultError(fault.code, fault.message);
      return impl.apply(receiver, args);
    };

  const wrapPort = (portLabel: string, port: object): unknown => {
    const cached = portProxies.get(port);
    if (cached) return cached;
    const proxy = new Proxy(port, {
      get(target, prop, _receiver) {
        const value = Reflect.get(target, prop, target);
        if (typeof value === "function")
          return methodWrapper(
            portLabel,
            String(prop),
            value as (...args: unknown[]) => unknown,
            target,
          );
        return value;
      },
    });
    portProxies.set(port, proxy);
    return proxy;
  };

  const wrapStore = (label: string, store: AssistantStore): AssistantStore =>
    new Proxy(store, {
      get(target, prop, _receiver) {
        if (prop === "transaction") {
          return methodWrapper(
            label,
            "transaction",
            ((fn: unknown) =>
              (target.transaction as (cb: unknown) => Promise<unknown>).call(
                target,
                (tx: AssistantStore) =>
                  (fn as (tx: AssistantStore) => unknown)(wrapStore(label, tx)),
              )) as (...args: unknown[]) => unknown,
            target,
          );
        }
        const value = Reflect.get(target, prop, target);
        if (typeof value === "function")
          return methodWrapper(
            label,
            String(prop),
            value as (...args: unknown[]) => unknown,
            target,
          );
        if (value !== null && typeof value === "object")
          return wrapPort(String(prop), value);
        return value;
      },
    });

  return {
    faulty: wrapStore("store", raw),
    raw,
    arm(key, spec = {}) {
      exact.set(key, {
        after: spec.after ?? 0,
        code: spec.code ?? "ENOSPC",
        message: spec.message ?? DEFAULT_MESSAGES[spec.code ?? "ENOSPC"],
      });
    },
    armPattern(pattern, spec = {}) {
      patterns.push({
        pattern,
        fault: {
          after: spec.after ?? 0,
          code: spec.code ?? "EIO",
          message: spec.message ?? DEFAULT_MESSAGES[spec.code ?? "EIO"],
        },
      });
    },
    clear() {
      exact.clear();
      patterns.length = 0;
    },
    calls(key) {
      if (typeof key === "string") return counts.get(key) ?? 0;
      let total = 0;
      for (const [name, count] of counts) if (key.test(name)) total += count;
      return total;
    },
    armed() {
      return [...exact.keys(), ...patterns.map((p) => String(p.pattern))];
    },
  };
}

/** A temp dir holding one sqlite file, removed when `cleanup()` runs. */
export interface SqliteScratch {
  dir: string;
  path: string;
  cleanup(): void;
}

export function createFaultScratch(label: string): SqliteScratch {
  const dir = mkdtempSync(join(tmpdir(), `agentkit-m0-fault-${label}-`));
  return {
    dir,
    path: join(dir, "store.sqlite"),
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  };
}
