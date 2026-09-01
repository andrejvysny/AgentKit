/**
 * `bun:sqlite`-backed {@link McpServerConfigStore} over the `mcp_servers` table
 * of {@link SCHEMA_V7}.
 *
 * STANDALONE, not a seventh member of {@link SqliteAssistantStore}. The
 * aggregate exists so writes that must land together can, and an MCP server
 * config shares a transaction with nothing — folding it in would force every
 * `AssistantStore` implementation, including the ones hosts hand-roll, to grow a
 * port most of them never use.
 *
 * It is constructed over the SAME DATABASE the assistant store uses — either by
 * handing it that store's `Database` handle (one connection, one write lock, no
 * second file to keep in step) or by naming the path, in which case it opens its
 * own handle exactly as the assistant store does. Both routes end at the same
 * `mcp_servers` table, and the second is what a host with no assistant store —
 * a config editor, a migration script — takes.
 *
 * NO SECRET MATERIAL IS WRITTEN. `secret_refs` holds `SecretStore` refs; the
 * values behind them are injected into env/header placeholders at connect time
 * and stored nowhere. See `secrets.ts` in `@agentkit/mcp-client`.
 */
import type { Database } from "bun:sqlite";
import { defaultClock, type Clock } from "@agentkit/host";
import {
  applyMcpServerConfigPatch,
  mcpConfigNotFound,
  mcpDuplicateAlias,
  mcpDuplicateId,
  type McpServerConfigPatch,
  type McpServerConfigRecord,
  type McpServerConfigStore,
  type McpTransportConfig,
} from "@agentkit/mcp-client";
import { openAgentKitDatabase } from "./sqlite-assistant-store.js";

interface McpServerRow {
  id: string;
  alias: string;
  transport: string;
  secret_refs: string | null;
  enabled: number | null;
  tool_aliases: string | null;
  resilience: string | null;
  created_at: string;
  updated_at: string;
}

export interface SqliteMcpServerConfigStoreOptions {
  /** Stamps `updatedAt` on a patch. Defaults to {@link defaultClock}. */
  clock?: Clock;
  /** Only consulted when a PATH is given; see {@link openAgentKitDatabase}. */
  busyTimeoutMs?: number;
}

function toJsonOrNull(value: unknown): string | null {
  return value === undefined ? null : JSON.stringify(value);
}

/**
 * A JSON column back to its value, or `undefined` for SQL NULL.
 *
 * `undefined` and not `null`: every optional field on `McpServerConfig` is
 * declared `field?: T`, so an absent one must come back absent — a `null` would
 * round-trip an unset `toolAliases` into a present-but-null property that
 * `Object.entries` would then walk.
 */
function fromJsonOrUndefined<T>(value: string | null): T | undefined {
  return value === null ? undefined : (JSON.parse(value) as T);
}

function recordFromRow(row: McpServerRow): McpServerConfigRecord {
  const secretRefs = fromJsonOrUndefined<Record<string, string>>(
    row.secret_refs,
  );
  const toolAliases = fromJsonOrUndefined<Record<string, string>>(
    row.tool_aliases,
  );
  const resilience = fromJsonOrUndefined<McpServerConfigRecord["resilience"]>(
    row.resilience,
  );
  return {
    id: row.id,
    alias: row.alias,
    transport: JSON.parse(row.transport) as McpTransportConfig,
    ...(secretRefs === undefined ? {} : { secretRefs }),
    // NULL is an ABSENT `enabled`, which the port reads as "default true" —
    // distinct from a stored `true`, and `isServerEnabled` treats them the
    // same. Collapsing the two here would silently rewrite the record.
    ...(row.enabled === null ? {} : { enabled: row.enabled !== 0 }),
    ...(toolAliases === undefined ? {} : { toolAliases }),
    ...(resilience === undefined ? {} : { resilience }),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

export class SqliteMcpServerConfigStore implements McpServerConfigStore {
  private readonly db: Database;
  private readonly clock: Clock;
  /** Whether this instance opened the handle, and may therefore close it. */
  private readonly ownsDb: boolean;

  constructor(
    db: Database | string,
    options: SqliteMcpServerConfigStoreOptions = {},
  ) {
    this.ownsDb = typeof db === "string";
    this.db =
      typeof db === "string"
        ? openAgentKitDatabase(db, options.busyTimeoutMs)
        : db;
    this.clock = options.clock ?? defaultClock;
  }

  /**
   * Closes the connection — but ONLY when this store opened it.
   *
   * Handed a `Database` by a caller, this is a no-op: the handle belongs to
   * whoever opened it (`SqliteAssistantStore`, normally), and closing another
   * object's connection out from under it would turn every later store call
   * into a driver error nobody could trace back to here.
   */
  close(): void {
    if (this.ownsDb) this.db.close();
  }

  async create(record: McpServerConfigRecord): Promise<McpServerConfigRecord> {
    return this.inTransaction(() => {
      if (this.rowById(record.id) !== null) throw mcpDuplicateId(record.id);
      if (this.idHoldingAlias(record.alias) !== null) {
        throw mcpDuplicateAlias(record.alias);
      }
      this.insert(record);
      return recordFromRow(this.requireRow(record.id));
    });
  }

  async update(
    id: string,
    patch: McpServerConfigPatch,
  ): Promise<McpServerConfigRecord> {
    return this.inTransaction(() => {
      const existing = recordFromRow(this.requireRow(id));
      // Re-stating a record's OWN alias is not a collision — a patch that
      // resends every field it read is the ordinary shape of an edit form, and
      // rejecting it would make "save" fail on a rename that never happened.
      if (patch.alias !== undefined && patch.alias !== existing.alias) {
        const holder = this.idHoldingAlias(patch.alias);
        if (holder !== null && holder !== id) {
          throw mcpDuplicateAlias(patch.alias);
        }
      }
      const next = applyMcpServerConfigPatch(
        existing,
        patch,
        this.clock.nowIso(),
      );
      this.run(
        `UPDATE mcp_servers SET
           alias = $alias, transport = $transport, secret_refs = $secretRefs,
           enabled = $enabled, tool_aliases = $toolAliases,
           resilience = $resilience, updated_at = $updatedAt
         WHERE id = $id`,
        { ...this.bindings(next), $id: id },
      );
      return recordFromRow(this.requireRow(id));
    });
  }

  async delete(id: string): Promise<void> {
    const changes = this.run(`DELETE FROM mcp_servers WHERE id = $id`, {
      $id: id,
    });
    if (changes === 0) throw mcpConfigNotFound(id);
  }

  async get(id: string): Promise<McpServerConfigRecord | null> {
    const row = this.rowById(id);
    return row === null ? null : recordFromRow(row);
  }

  async list(): Promise<McpServerConfigRecord[]> {
    const rows = this.db
      .query(`SELECT * FROM mcp_servers ORDER BY created_at ASC, id ASC`)
      .all() as McpServerRow[];
    return rows.map(recordFromRow);
  }

  /**
   * The check and the write in one `BEGIN IMMEDIATE`, so the uniqueness a
   * caller is told about is the uniqueness that was committed.
   *
   * Flattens rather than nests when a transaction is already open on this
   * handle — the assistant store's `transaction()` holds one, and `bun:sqlite`
   * has no savepoints in this version, so a second BEGIN would raise ("cannot
   * start a transaction within a transaction") instead of joining. The open
   * transaction is read off the driver's own `Database.inTransaction` rather
   * than off a counter this class keeps: the transaction may have been opened
   * by the assistant store sharing this handle, which no counter here could
   * know about.
   */
  private inTransaction<T>(fn: () => T): T {
    if (this.db.inTransaction) return fn();
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const result = fn();
      this.db.exec("COMMIT");
      return result;
    } catch (err) {
      try {
        this.db.exec("ROLLBACK");
      } catch {
        // Already rolled back by SQLite (a constraint failure can do it):
        // rethrowing the rollback would hide the error that caused it.
      }
      throw err;
    }
  }

  private insert(record: McpServerConfigRecord): void {
    this.run(
      `INSERT INTO mcp_servers
         (id, alias, transport, secret_refs, enabled, tool_aliases, resilience, created_at, updated_at)
       VALUES ($id, $alias, $transport, $secretRefs, $enabled, $toolAliases, $resilience, $createdAt, $updatedAt)`,
      {
        ...this.bindings(record),
        $id: record.id,
        $createdAt: record.createdAt,
      },
    );
  }

  /** The columns an insert and an update share, from one record. */
  private bindings(record: McpServerConfigRecord): Record<string, unknown> {
    return {
      $alias: record.alias,
      $transport: JSON.stringify(record.transport),
      $secretRefs: toJsonOrNull(record.secretRefs),
      $enabled: record.enabled === undefined ? null : record.enabled ? 1 : 0,
      $toolAliases: toJsonOrNull(record.toolAliases),
      $resilience: toJsonOrNull(record.resilience),
      $updatedAt: record.updatedAt,
    };
  }

  private rowById(id: string): McpServerRow | null {
    const row = this.db
      .query(`SELECT * FROM mcp_servers WHERE id = $id`)
      .get({ $id: id }) as McpServerRow | null;
    return row ?? null;
  }

  private requireRow(id: string): McpServerRow {
    const row = this.rowById(id);
    if (row === null) throw mcpConfigNotFound(id);
    return row;
  }

  /** The id holding this alias, or null. Case-SENSITIVE; see the port doc. */
  private idHoldingAlias(alias: string): string | null {
    const row = this.db
      .query(`SELECT id FROM mcp_servers WHERE alias = $alias`)
      .get({ $alias: alias }) as { id: string } | null;
    return row?.id ?? null;
  }

  private run(sql: string, params: Record<string, unknown>): number {
    // bun-types' generic for `Database.run` models an array of binding ARRAYS,
    // which does not match its own documented single-object calling convention
    // — the same mismatch `SqliteConnection.run` works around, and for the same
    // reason it stays a METHOD call on the instance (the native binding needs
    // `this`).
    const db = this.db as unknown as {
      run(sql: string, params: Record<string, unknown>): { changes: number };
    };
    return db.run(sql, params).changes;
  }
}
