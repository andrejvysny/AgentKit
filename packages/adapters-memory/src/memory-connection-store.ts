import type { ConnectionRecord, ConnectionStore } from "@agentkit/mcp-client";
import { McpError } from "@agentkit/mcp-client";

export class MemoryConnectionStore implements ConnectionStore {
  private readonly records = new Map<string, ConnectionRecord>();
  async get(id: string) {
    return structuredClone(this.records.get(id) ?? null);
  }
  async list() {
    return structuredClone([...this.records.values()]);
  }
  async put(record: ConnectionRecord, expectedRevision: number | null) {
    if ((this.records.get(record.id)?.revision ?? null) !== expectedRevision)
      throw new McpError(
        "mcp_stale_connection",
        "Connection revision conflict.",
      );
    this.records.set(record.id, structuredClone(record));
  }
  async delete(id: string) {
    this.records.delete(id);
  }
}
