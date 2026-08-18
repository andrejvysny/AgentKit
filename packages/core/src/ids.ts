export function newRunId(): string {
  return `run_${crypto.randomUUID()}`;
}

export function newToolEventId(): string {
  return `tev_${crypto.randomUUID()}`;
}

/**
 * Identifies one provider call. A run makes several — every tool round-trip is
 * another billed request — so usage and retries are attributed per call, not per
 * run.
 */
export function newCallId(): string {
  return `call_${crypto.randomUUID()}`;
}

/** Identifies a single run event; the key consumers dedup on when replaying. */
export function newEventId(): string {
  return `evt_${crypto.randomUUID()}`;
}

export function nowIso(): string {
  return new Date().toISOString();
}
