import Database from "better-sqlite3";
import {
  normalizeBindings,
  normalizeChanges,
  normalizeRow,
  type SqliteDatabase,
  type SqliteParams,
  type SqliteStatement,
} from "./driver.js";

function bindings(params: SqliteParams): SqliteParams {
  const normalized = Object.create(null) as SqliteParams;
  for (const [key, value] of Object.entries(normalizeBindings(params))) {
    const name = key.replace(/^[$:@]/, "");
    if (Object.hasOwn(normalized, name))
      throw new TypeError(`Ambiguous SQLite parameter ${name}`);
    normalized[name] = value;
  }
  return normalized;
}

export class NodeSqliteDatabase implements SqliteDatabase {
  private readonly handle: Database.Database;

  constructor(path: string, nativeBinding?: string) {
    this.handle = new Database(path, { nativeBinding });
    this.handle.defaultSafeIntegers(true);
  }

  query(sql: string): SqliteStatement {
    const statement = this.handle.prepare(sql);
    return {
      get: (params) =>
        normalizeRow(
          params === undefined
            ? statement.get()
            : statement.get(bindings(params)),
        ),
      all: (params) =>
        (params === undefined
          ? statement.all()
          : statement.all(bindings(params))
        ).map(normalizeRow),
    };
  }

  run(sql: string, params?: SqliteParams) {
    const statement = this.handle.prepare(sql);
    return normalizeChanges(
      params === undefined ? statement.run() : statement.run(bindings(params)),
    );
  }

  exec(sql: string): void {
    this.handle.exec(sql);
  }
  close(): void {
    this.handle.close();
  }
}
