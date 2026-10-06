import type { SqliteDatabase } from "./sqlite/driver.js";
import { openAgentKitDatabase } from "./sqlite-assistant-store.js";
import {
  SharedSqliteMcpServerConfigStore,
  type SqliteMcpServerConfigStoreOptions,
} from "./shared-mcp-server-config-store.js";
export type { SqliteMcpServerConfigStoreOptions } from "./shared-mcp-server-config-store.js";
export class SqliteMcpServerConfigStore extends SharedSqliteMcpServerConfigStore {
  constructor(
    db: SqliteDatabase | string,
    options: SqliteMcpServerConfigStoreOptions = {},
  ) {
    super(
      typeof db === "string"
        ? (openAgentKitDatabase(
            db,
            options.busyTimeoutMs,
          ) as unknown as SqliteDatabase)
        : db,
      options,
      typeof db === "string",
    );
  }
}
