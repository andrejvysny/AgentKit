/** Why a cassette operation failed. Stable strings; safe to assert on in tests. */
export type CassetteErrorCode =
  /** Fixture failed schema validation. */
  | "cassette_schema"
  /** Replay asked for an effect the fixture does not contain at this position. */
  | "cassette_miss"
  /** Effect matched identity but not the recorded request/result fingerprint. */
  | "cassette_mismatch"
  /** Strict mode: recorded effects remained unconsumed at close. */
  | "cassette_extra"
  /** A recording/verification was finalized twice or used after close. */
  | "cassette_closed"
  /** Replay attempted a live provider/tool call — always a test failure. */
  | "cassette_live_call"
  /** Replay needs catalog data (models/capabilities) the fixture lacks. */
  | "cassette_no_catalog";

export class CassetteError extends Error {
  readonly code: CassetteErrorCode;
  readonly details?: Record<string, unknown>;

  constructor(
    code: CassetteErrorCode,
    message: string,
    details?: Record<string, unknown>,
  ) {
    super(`cassette[${code}]: ${message}`);
    this.name = "CassetteError";
    this.code = code;
    this.details = details;
  }
}
