/**
 * `bun:sqlite`-backed {@link SettingsStore}: the single-row `settings` table.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import type {
  AssistantSettings,
  SettingsStore,
  ToolCallingMode,
} from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import {
  type SettingsRow,
  settingsFromRow,
  toIntBool,
  toJson,
} from "./rows.js";

/** Mirrors the `settings.tool_calling_mode` DDL default. */
const DEFAULT_TOOL_CALLING_MODE: ToolCallingMode = "auto";

export class SqliteSettingsStore implements SettingsStore {
  constructor(
    private readonly conn: SqliteConnection,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async getSettings(): Promise<AssistantSettings> {
    const row = this.conn.get(
      `SELECT * FROM settings WHERE id = 1`,
    ) as SettingsRow;
    return settingsFromRow(row);
  }

  async updateSettings(
    patch: Partial<AssistantSettings>,
  ): Promise<AssistantSettings> {
    return this.conn.whenFree(() => {
      const row = this.conn.get(
        `SELECT * FROM settings WHERE id = 1`,
      ) as SettingsRow;
      const merged: AssistantSettings = { ...settingsFromRow(row), ...patch };
      this.conn.run(
        `UPDATE settings SET
           default_provider_id = $defaultProviderId, default_model = $defaultModel,
           context_size_preference = $contextSizePreference, write_policy_mode = $writePolicyMode,
           allow_raw_tool_data = $allowRawToolData, max_tool_iterations = $maxToolIterations,
           tool_calling_mode = $toolCallingMode, metadata = $metadata
         WHERE id = 1`,
        {
          $defaultProviderId: merged.defaultProviderId ?? null,
          $defaultModel: merged.defaultModel ?? null,
          $contextSizePreference: merged.contextSizePreference,
          $writePolicyMode: merged.writePolicyMode,
          $allowRawToolData: toIntBool(merged.allowRawToolData),
          $maxToolIterations: merged.maxToolIterations ?? null,
          // The column is NOT NULL: an explicit `undefined` in the patch means
          // "back to the default", not "store nothing".
          $toolCallingMode: merged.toolCalling ?? DEFAULT_TOOL_CALLING_MODE,
          $metadata: toJson(merged.metadata),
        },
      );
      return merged;
    }, this.txOwner);
  }
}
