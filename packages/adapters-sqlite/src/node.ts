import {
  SharedSqliteAssistantStore,
  type SqliteAssistantStoreOptions,
} from "./shared-assistant-store.js";
import { NodeSqliteDatabase } from "./sqlite/node-driver.js";
import { initializeDatabase } from "./sqlite/open.js";
export * from "./schema.js";
export type {
  SqliteAssistantStoreOptions,
  SqliteWriteGate,
} from "./shared-assistant-store.js";
export { writeGateFor } from "./shared-assistant-store.js";

export interface NodeSqliteAssistantStoreOptions
  extends SqliteAssistantStoreOptions {
  /** Optional explicit addon path for applications that relocate native binaries. */
  nativeBinding?: string;
}

export function openNodeAgentKitDatabase(
  path: string,
  busyTimeoutMs?: number,
  nativeBinding?: string,
): NodeSqliteDatabase {
  const db = new NodeSqliteDatabase(path, nativeBinding);
  initializeDatabase(db, path, busyTimeoutMs);
  return db;
}

export class NodeSqliteAssistantStore extends SharedSqliteAssistantStore {
  constructor(path: string, options: NodeSqliteAssistantStoreOptions = {}) {
    super(
      openNodeAgentKitDatabase(
        path,
        options.busyTimeoutMs,
        options.nativeBinding,
      ),
      options,
    );
  }
}

import {
  SharedSqliteMcpServerConfigStore,
  type SqliteMcpServerConfigStoreOptions,
} from "./shared-mcp-server-config-store.js";
import type { SqliteDatabase } from "./sqlite/driver.js";
export type { SqliteMcpServerConfigStoreOptions } from "./shared-mcp-server-config-store.js";
export class NodeSqliteMcpServerConfigStore extends SharedSqliteMcpServerConfigStore {
  constructor(
    db: SqliteDatabase | string,
    options: SqliteMcpServerConfigStoreOptions = {},
  ) {
    super(
      typeof db === "string"
        ? openNodeAgentKitDatabase(db, options.busyTimeoutMs)
        : db,
      options,
      typeof db === "string",
    );
  }
}
