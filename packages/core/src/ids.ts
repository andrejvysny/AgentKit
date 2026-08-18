export function newRunId(): string {
  return `run_${crypto.randomUUID()}`;
}

export function newToolCallId(): string {
  return `call_${crypto.randomUUID()}`;
}

export function newToolEventId(): string {
  return `tev_${crypto.randomUUID()}`;
}

export function nowIso(): string {
  return new Date().toISOString();
}
