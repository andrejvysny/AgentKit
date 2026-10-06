import { Database } from "bun:sqlite";
import {
  SharedSqliteAssistantStore,
  type SqliteAssistantStoreOptions,
} from "./shared-assistant-store.js";
import { initializeDatabase } from "./sqlite/open.js";
import type { SqliteDatabase } from "./sqlite/driver.js";
export {
  writeGateFor,
  type SqliteWriteGate,
  type SqliteAssistantStoreOptions,
} from "./shared-assistant-store.js";

export function openAgentKitDatabase(
  path: string,
  busyTimeoutMs?: number,
): Database {
  const db = new Database(path, { safeIntegers: true });
  initializeDatabase(db as unknown as SqliteDatabase, path, busyTimeoutMs);
  return db;
}

export class SqliteAssistantStore extends SharedSqliteAssistantStore {
  constructor(path: string, options: SqliteAssistantStoreOptions = {}) {
    super(
      openAgentKitDatabase(
        path,
        options.busyTimeoutMs,
      ) as unknown as SqliteDatabase,
      options,
    );
  }
}
