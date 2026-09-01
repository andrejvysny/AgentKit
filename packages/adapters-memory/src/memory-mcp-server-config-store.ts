/**
 * Map-backed {@link McpServerConfigStore} — the reference implementation of the
 * MCP server-config port, and the one a test or a local dev host wires.
 *
 * STANDALONE, not a seventh member of `MemoryAssistantStore`. The aggregate
 * exists so writes that must land together can, and an MCP server config shares
 * a transaction with nothing; folding it in would force every `AssistantStore`
 * implementation — including the ones hosts hand-roll — to grow a port most of
 * them never use. It is constructed beside the store, not inside it.
 *
 * SNAPSHOT RETURNS, same rule the rest of this package follows: nothing a caller
 * receives or hands over is an object this store keeps a reference to, so
 * mutating a returned record cannot edit what is stored (and a caller that keeps
 * the record it passed to `create` cannot edit it either).
 */
import { defaultClock, type Clock } from "@agentkit/host";
import {
  applyMcpServerConfigPatch,
  mcpConfigNotFound,
  mcpDuplicateAlias,
  mcpDuplicateId,
  type McpServerConfigPatch,
  type McpServerConfigRecord,
  type McpServerConfigStore,
} from "@agentkit/mcp-client";

export interface MemoryMcpServerConfigStoreOptions {
  /** Stamps `updatedAt` on a patch. Defaults to {@link defaultClock}. */
  clock?: Clock;
}

/**
 * A record copied deeply enough that the store and its callers cannot reach
 * each other's objects.
 *
 * `structuredClone` rather than a spread: a config's `env`, `headers`,
 * `secretRefs`, `toolAliases` and `resilience` are all nested objects, and a
 * shallow copy would hand out the very bags this store keeps — so a caller
 * adding an environment variable to a record it listed would silently edit the
 * stored config. The sqlite adapter gets this for free by rebuilding from JSON
 * on every read; a Map-backed store has to mean it.
 */
function clone(record: McpServerConfigRecord): McpServerConfigRecord {
  return structuredClone(record);
}

export class MemoryMcpServerConfigStore implements McpServerConfigStore {
  /** Insertion-ordered by construction, which is `createdAt` ascending. */
  private readonly records = new Map<string, McpServerConfigRecord>();
  private readonly clock: Clock;

  constructor(options: MemoryMcpServerConfigStoreOptions = {}) {
    this.clock = options.clock ?? defaultClock;
  }

  async create(record: McpServerConfigRecord): Promise<McpServerConfigRecord> {
    if (this.records.has(record.id)) throw mcpDuplicateId(record.id);
    if (this.aliasHolder(record.alias) !== null) {
      throw mcpDuplicateAlias(record.alias);
    }
    this.records.set(record.id, clone(record));
    return clone(record);
  }

  async update(
    id: string,
    patch: McpServerConfigPatch,
  ): Promise<McpServerConfigRecord> {
    const existing = this.records.get(id);
    if (existing === undefined) throw mcpConfigNotFound(id);
    // Re-stating a record's OWN alias is not a collision — a patch that
    // resends every field it read is the ordinary shape of an edit form, and
    // rejecting it would make "save" fail on a rename that never happened.
    if (
      patch.alias !== undefined &&
      patch.alias !== existing.alias &&
      this.aliasHolder(patch.alias) !== null
    ) {
      throw mcpDuplicateAlias(patch.alias);
    }
    const next = applyMcpServerConfigPatch(
      existing,
      patch,
      this.clock.nowIso(),
    );
    this.records.set(id, clone(next));
    return clone(next);
  }

  async delete(id: string): Promise<void> {
    if (!this.records.delete(id)) throw mcpConfigNotFound(id);
  }

  async get(id: string): Promise<McpServerConfigRecord | null> {
    const record = this.records.get(id);
    return record === undefined ? null : clone(record);
  }

  async list(): Promise<McpServerConfigRecord[]> {
    return [...this.records.values()]
      .sort((a, b) => a.createdAt.localeCompare(b.createdAt))
      .map(clone);
  }

  /** The id holding this alias, or null. Case-SENSITIVE; see the port doc. */
  private aliasHolder(alias: string): string | null {
    for (const record of this.records.values()) {
      if (record.alias === alias) return record.id;
    }
    return null;
  }
}
