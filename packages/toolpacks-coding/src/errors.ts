/**
 * A user-facing failure: the tool turns it into an `ok:false` result with the
 * code and message, never into a throw that fails the run.
 */
export class CodingError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "CodingError";
    this.code = code;
  }
}
