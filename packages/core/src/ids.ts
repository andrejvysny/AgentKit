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

export function nowIso(): string {
  return new Date().toISOString();
}
