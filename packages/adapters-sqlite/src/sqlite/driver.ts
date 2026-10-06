export type SqliteValue =
  | string
  | number
  | boolean
  | bigint
  | null
  | Uint8Array;
export type SqliteParams = Record<string, SqliteValue>;

export interface SqliteChanges {
  changes: number;
  lastInsertRowid: number | bigint;
}

export interface SqliteStatement {
  get(params?: SqliteParams): unknown;
  all(params?: SqliteParams): unknown[];
}

/** Synchronous SQL only; async transaction ownership belongs to the shared gate. */
export interface SqliteDatabase {
  query(sql: string): SqliteStatement;
  run(sql: string, params?: SqliteParams): SqliteChanges;
  exec(sql: string): unknown;
  close(): void;
}

export function normalizeBindings(params: SqliteParams): SqliteParams {
  return Object.fromEntries(
    Object.entries(params).map(([key, value]) => [
      key,
      typeof value === "boolean" ? Number(value) : value,
    ]),
  );
}

export function safeInteger(value: number | bigint): number {
  const number = Number(value);
  if (!Number.isSafeInteger(number))
    throw new RangeError("SQLite integer exceeds the safe integer range");
  return number;
}

export function normalizeChanges(value: SqliteChanges): SqliteChanges {
  return {
    changes: safeInteger(value.changes),
    lastInsertRowid: value.lastInsertRowid,
  };
}

export function normalizeRow(value: unknown): unknown {
  if (value === undefined || value === null) return null;
  if (typeof value !== "object" || value instanceof Uint8Array) return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, cell]) => [
      key,
      typeof cell === "bigint" ? safeInteger(cell) : cell,
    ]),
  );
}
