import {
  McpError,
  type ConnectionRecord,
  type ConnectionStore,
} from "@agentkit/mcp-client";
import type { SqliteDatabase } from "./sqlite/driver.js";
import { writeGateFor } from "./shared-assistant-store.js";

/** Optional standalone store on a caller-owned AgentKit database handle. */
export class SharedSqliteConnectionStore implements ConnectionStore {
  constructor(private readonly db: SqliteDatabase) {
    db.exec(
      "CREATE TABLE IF NOT EXISTS connector_connections (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL)",
    );
  }
  async get(id: string): Promise<ConnectionRecord | null> {
    const row = this.db
      .query("SELECT record FROM connector_connections WHERE id = $id")
      .get({ $id: id }) as { record: string } | null;
    return row ? JSON.parse(row.record) : null;
  }
  async list(): Promise<ConnectionRecord[]> {
    return (
      this.db
        .query("SELECT record FROM connector_connections ORDER BY id")
        .all() as { record: string }[]
    ).map((row) => JSON.parse(row.record));
  }
  async put(record: ConnectionRecord, expectedRevision: number | null) {
    await writeGateFor(this.db).whenFree(() => {
      const params = {
        $id: record.id,
        $revision: record.revision,
        $record: JSON.stringify(record),
      };
      if (expectedRevision === null) {
        const result = this.db.run(
          "INSERT OR IGNORE INTO connector_connections (id, revision, record) VALUES ($id, $revision, $record)",
          params,
        );
        if (Number(result.changes) !== 1)
          throw new McpError(
            "mcp_stale_connection",
            "Connection revision conflict.",
          );
      } else {
        const result = this.db.run(
          "UPDATE connector_connections SET revision = $revision, record = $record WHERE id = $id AND revision = $expected",
          { ...params, $expected: expectedRevision },
        );
        if (Number(result.changes) !== 1)
          throw new McpError(
            "mcp_stale_connection",
            "Connection revision conflict.",
          );
      }
    });
  }
  async delete(id: string) {
    await writeGateFor(this.db).whenFree(() => {
      this.db.run("DELETE FROM connector_connections WHERE id = $id", {
        $id: id,
      });
    });
  }
}
