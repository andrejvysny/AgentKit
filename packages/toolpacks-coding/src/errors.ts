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

/** The session's last-read version no longer matches the file on disk. */
export function fileChangedError(label: string): CodingError {
  return new CodingError(
    "file_changed",
    `${label} changed since this session last read it (another session or an external editor modified it). Read it again and redo the edit.`,
  );
}

/** write_file would overwrite an existing file this session never read. */
export function fileNotReadError(label: string, partial = false): CodingError {
  return new CodingError(
    "file_not_read",
    partial
      ? `${label} already exists and you only read part of it. Read the whole file (no offset/limit, nothing truncated) or pass expectedSha256 before overwriting it.`
      : `${label} already exists. Read it before overwriting it.`,
  );
}

/** write_file lost a create race: the file appeared before our link. */
export function fileCreatedElsewhereError(label: string): CodingError {
  return new CodingError(
    "file_changed",
    `${label} was created by someone else while writing it. Read it, then edit or overwrite it.`,
  );
}
