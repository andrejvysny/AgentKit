/**
 * Persistence for {@link McpServerConfig} — the port a host implements so its
 * MCP servers survive a restart and can be managed from a UI.
 *
 * It is deliberately NOT part of `AssistantStore`. That aggregate exists so the
 * writes that must land together can (a turn's task and its two messages, a
 * run's events and its status); an MCP server config shares a transaction with
 * nothing, and adding a seventh store to the aggregate would make every adapter
 * — and every host that hand-rolls one — implement a port most of them do not
 * use. A host wires this one only if it lets a user add MCP servers at runtime;
 * one that declares its servers in a config file never needs it.
 *
 * NO SECRET MATERIAL IS STORED HERE, for the same reason
 * {@link McpServerConfig} carries none: a record is listed, logged and shown in
 * a UI. `secretRefs` names {@link SecretStore} refs, and the values are injected
 * into env/header placeholders at connect time and nowhere else (see
 * `secrets.ts`).
 */
import { McpError } from "./errors.js";
import type { McpServerConfig } from "./config.js";

/**
 * A stored server config: the declaration plus the identity and timestamps a
 * store owns.
 *
 * `id` is separate from `alias` on purpose, even though both are unique. The
 * alias is the tool NAMESPACE — it is baked into every canonical tool id
 * (`mcp.<alias>.<tool>`) a transcript records — and a host that renames one is
 * making a visible change to its own namespace. `id` is the handle a URL and a
 * foreign key use, and it must survive that rename.
 */
export interface McpServerConfigRecord extends McpServerConfig {
  id: string;
  /** ISO-8601. */
  createdAt: string;
  /** ISO-8601. */
  updatedAt: string;
}

/**
 * Fields an update may change.
 *
 * FIELD-LEVEL REPLACE, not a deep merge: a present `env`, `headers`,
 * `secretRefs` or `toolAliases` replaces the stored bag wholesale. A merge makes
 * "remove this variable" unexpressible, and a caller that wanted one already has
 * the record it read to build the replacement from.
 *
 * `id`, `createdAt` and `updatedAt` are absent because a store owns all three.
 */
export type McpServerConfigPatch = Partial<McpServerConfig>;

/**
 * CRUD over stored MCP server configs.
 *
 * ALIAS UNIQUENESS is the one invariant an implementation must enforce, and it
 * is enforced CASE-SENSITIVELY — the alias grammar (`^[a-z][a-z0-9-]*$`) has no
 * uppercase in it, so a case-insensitive comparison would only ever differ on
 * values that cannot be connected anyway. Two servers sharing an alias would
 * mint the same canonical id for two different tools, which
 * `resolveMcpToolIdentity` refuses at staging time — as a hard failure of the
 * whole run, long after the record that caused it was written. Refusing the
 * write is the same rule applied where it can still be acted on.
 */
export interface McpServerConfigStore {
  /**
   * Write a new record verbatim — the caller owns `id`, `createdAt` and
   * `updatedAt`, because a host importing its config file's servers has ids
   * already pointing at them.
   *
   * A duplicate `id` or a duplicate `alias` is `mcp_invalid_config`.
   */
  create(record: McpServerConfigRecord): Promise<McpServerConfigRecord>;
  /**
   * Apply a patch and answer with the record as it now stands.
   *
   * Unknown id → `mcp_config_not_found`; an `alias` already held by ANOTHER
   * record → `mcp_invalid_config`. Re-stating a record's own alias is not a
   * collision.
   */
  update(
    id: string,
    patch: McpServerConfigPatch,
  ): Promise<McpServerConfigRecord>;
  /** Unknown id → `mcp_config_not_found`, rather than a silent success. */
  delete(id: string): Promise<void>;
  /** Null when nothing has this id — an absent config is not an error. */
  get(id: string): Promise<McpServerConfigRecord | null>;
  /** Every stored config, `createdAt` ascending. */
  list(): Promise<McpServerConfigRecord[]>;
}

/** The `mcp_config_not_found` an implementation raises for an unknown id. */
export function mcpConfigNotFound(id: string): McpError {
  return new McpError(
    "mcp_config_not_found",
    `No MCP server config with id "${id}".`,
    { details: { id } },
  );
}

/** The `mcp_invalid_config` an implementation raises for a taken alias. */
export function mcpDuplicateAlias(alias: string): McpError {
  return new McpError(
    "mcp_invalid_config",
    `An MCP server config with alias "${alias}" already exists; aliases are the tool namespace and must be unique.`,
    { details: { alias } },
  );
}

/** The `mcp_invalid_config` an implementation raises for a taken id. */
export function mcpDuplicateId(id: string): McpError {
  return new McpError(
    "mcp_invalid_config",
    `An MCP server config with id "${id}" already exists.`,
    { details: { id } },
  );
}

/**
 * The three fields a STORE owns, and no patch may move.
 *
 * `McpServerConfigPatch` already excludes them at the type level; this is the
 * runtime half, because a patch that reached an implementation is a value that
 * crossed a boundary — a parsed request body, a config file — and a `Partial<>`
 * proves nothing about what is actually in the object. Silently re-keying a
 * record under a caller-supplied `id` is the one mistake here that would look
 * like a successful update.
 */
const STORE_OWNED_FIELDS = new Set(["id", "createdAt", "updatedAt"]);

/**
 * The record a patch produces, with `updatedAt` stamped.
 *
 * Shared by every implementation so the field-level-replace rule above is
 * written once: an adapter that spread the patch itself would have to remember
 * that `undefined` in a partial means "not mentioned" and not "clear it", and
 * the two adapters would eventually disagree about which.
 */
export function applyMcpServerConfigPatch(
  record: McpServerConfigRecord,
  patch: McpServerConfigPatch,
  updatedAt: string,
): McpServerConfigRecord {
  const next: McpServerConfigRecord = { ...record, updatedAt };
  for (const [key, value] of Object.entries(patch)) {
    if (value === undefined || STORE_OWNED_FIELDS.has(key)) continue;
    (next as unknown as Record<string, unknown>)[key] = value;
  }
  return next;
}
