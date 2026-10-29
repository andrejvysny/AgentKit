import {
  identityKey,
  inspectProcess,
  signalIdentity,
  type ProcessDiscovery,
  type ProcessIdentity,
  type ProcessInspection,
} from "./process-identity.js";

export interface Teardown {
  state: "stopped" | "survivors" | "unknown";
  escalated: boolean;
  survivors: number[];
  scan: "ok" | "unsupported";
}

export interface TerminateSpec {
  find: () => Promise<ProcessDiscovery | null>;
  initial?: ProcessIdentity[];
  guardian?: ProcessIdentity;
  graceMs: number;
  inspect?: (identity: ProcessIdentity) => Promise<ProcessInspection>;
  send?: (pid: number, signal: NodeJS.Signals) => void;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
  survivorWaitMs?: number;
  onSeal?: (
    discovery: ProcessDiscovery & { closed: false; sealed: true },
  ) => Promise<void>;
  onFinal?: (
    discovery: ProcessDiscovery & { closed: boolean },
  ) => Promise<void>;
}

class CleanupCensus {
  readonly known = new Map<string, ProcessIdentity>();
  incomplete = false;
  private readonly inspect;

  constructor(private readonly spec: TerminateSpec) {
    this.inspect = spec.inspect ?? inspectProcess;
    for (const identity of spec.initial ?? [])
      this.known.set(identityKey(identity), identity);
  }

  async refresh(): Promise<void> {
    const found = await this.spec.find().catch(() => null);
    if (!found || !found.complete) this.incomplete = true;
    for (const identity of found?.identities ?? [])
      this.known.set(identityKey(identity), identity);
  }

  async states(
    excludeGuardian = false,
  ): Promise<{ live: ProcessIdentity[]; unknown: boolean; frozen: boolean }> {
    const live: ProcessIdentity[] = [];
    let unknown = false;
    let frozen = true;
    for (const identity of this.known.values()) {
      if (
        excludeGuardian &&
        this.spec.guardian &&
        identityKey(identity) === identityKey(this.spec.guardian)
      )
        continue;
      const inspected = await this.inspect(identity).catch(
        (): ProcessInspection => ({ state: "unknown" }),
      );
      if (inspected.state === "same-live") {
        live.push(identity);
        frozen &&= inspected.observation?.stopped === true;
      }
      if (inspected.state === "unknown") unknown = true;
    }
    if (unknown) this.incomplete = true;
    return { live, unknown, frozen };
  }

  async signal(signal: NodeJS.Signals, excludeGuardian = false): Promise<void> {
    for (const identity of this.known.values()) {
      if (
        excludeGuardian &&
        this.spec.guardian &&
        identityKey(identity) === identityKey(this.spec.guardian)
      )
        continue;
      const result = await signalIdentity(
        identity,
        signal,
        this.inspect,
        this.spec.send,
      );
      if (result.state === "unknown") this.incomplete = true;
    }
  }

  async freeze(): Promise<boolean> {
    for (let attempt = 0; attempt < 3; attempt++) {
      await this.signal("SIGSTOP");
      const before = this.known.size;
      await this.refresh();
      const states = await this.states();
      const guardian = this.spec.guardian
        ? await this.inspect(this.spec.guardian).catch(
            (): ProcessInspection => ({ state: "unknown" }),
          )
        : undefined;
      const anchored =
        !guardian ||
        (guardian.state === "same-live" &&
          guardian.observation?.stopped === true);
      if (
        before === this.known.size &&
        states.frozen &&
        !states.unknown &&
        anchored
      )
        return !this.incomplete;
    }
    this.incomplete = true;
    return false;
  }

  async wait(ms: number, excludeGuardian = false): Promise<void> {
    const now = this.spec.now ?? Date.now;
    const sleep =
      this.spec.sleep ??
      ((delay: number) =>
        new Promise<void>((resolve) => setTimeout(resolve, delay)));
    const deadline = now() + ms;
    while (
      (await this.states(excludeGuardian)).live.length > 0 &&
      now() < deadline
    )
      await sleep(25);
  }
}

/** No numeric process-group signals: each discovered member is identity-checked. */
export async function terminate(
  spec: TerminateSpec,
): Promise<Teardown & { seen: number }> {
  const census = new CleanupCensus(spec);
  await census.refresh();
  for (
    let attempt = 0;
    attempt < 3 && (await census.states()).live.length > 0;
    attempt++
  ) {
    await census.signal("SIGSTOP");
    const before = census.known.size;
    await census.refresh();
    if (census.known.size === before) break;
    if (attempt === 2) census.incomplete = true;
  }
  await census.signal("SIGTERM", true);
  await census.signal("SIGCONT", true);
  await census.wait(spec.graceMs, true);
  const escalated = (await census.states(true)).live.length > 0;
  let sealed = false;
  let sealedKeys: Set<string> | undefined;
  if (spec.guardian) {
    if (await census.freeze()) {
      try {
        await spec.onSeal?.({
          identities: [...census.known.values()],
          complete: true,
          closed: false,
          sealed: true,
        });
        sealed = true;
        sealedKeys = new Set(census.known.keys());
        await census.signal("SIGKILL");
        await census.wait(spec.survivorWaitMs ?? 500);
      } catch {
        census.incomplete = true;
      }
    }
  } else if (escalated) {
    await census.refresh();
    await census.signal("SIGKILL");
    await census.wait(spec.survivorWaitMs ?? 500);
  }
  await census.refresh();
  if (sealed && [...census.known.keys()].some((key) => !sealedKeys?.has(key))) {
    sealed = false;
    census.incomplete = true;
  }
  const final = await census.states();
  const state =
    final.live.length > 0
      ? "survivors"
      : census.incomplete
        ? "unknown"
        : "stopped";
  try {
    await spec.onFinal?.({
      identities: [...census.known.values()],
      complete: sealed || !census.incomplete,
      closed: state === "stopped",
      sealed,
    });
  } catch {
    census.incomplete = true;
  }
  return {
    state: state === "stopped" && census.incomplete ? "unknown" : state,
    escalated,
    survivors: [...new Set(final.live.map(({ pid }) => pid))],
    scan: census.incomplete ? "unsupported" : "ok",
    seen: census.known.size,
  };
}
