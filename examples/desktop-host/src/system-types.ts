/** Wall-clock access; the shape `@agentkit/native` accepts for its injected `clock`. */
export interface Clock {
  now(): Date;
  /** ISO-8601 rendering of `now()`. */
  nowIso(): string;
}

export interface Logger {
  debug(message: string, fields?: Record<string, unknown>): void;
  info(message: string, fields?: Record<string, unknown>): void;
  warn(message: string, fields?: Record<string, unknown>): void;
  error(message: string, fields?: Record<string, unknown>): void;
}
