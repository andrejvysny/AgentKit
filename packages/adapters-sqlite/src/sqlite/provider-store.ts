/**
 * `bun:sqlite`-backed {@link ProviderStore}: provider configs, their cached
 * model lists, and cached capability probes.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import type {
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
} from "@agentkit/contracts";
import type { ProviderStore } from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import {
  capabilitiesFromRow,
  modelFromRow,
  type ProviderCapabilitiesRow,
  providerFromRow,
  type ProviderModelRow,
  type ProviderRow,
  toIntBool,
  toJson,
  toOptionalIntBool,
} from "./rows.js";

export class SqliteProviderStore implements ProviderStore {
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

  async listProviders(): Promise<AiProviderConfig[]> {
    const rows = this.conn.all(
      `SELECT * FROM providers ORDER BY rowid ASC`,
    ) as ProviderRow[];
    return rows.map(providerFromRow);
  }

  async getProvider(providerId: string): Promise<AiProviderConfig | null> {
    const row = this.conn.get(`SELECT * FROM providers WHERE id = $id`, {
      $id: providerId,
    }) as ProviderRow | undefined;
    return row ? providerFromRow(row) : null;
  }

  async upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig> {
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO providers (id, label, kind, base_url, api_key, default_model, enabled, extra_headers, metadata)
         VALUES ($id, $label, $kind, $baseUrl, $apiKey, $defaultModel, $enabled, $extraHeaders, $metadata)
         ON CONFLICT(id) DO UPDATE SET
           label = excluded.label, kind = excluded.kind, base_url = excluded.base_url,
           api_key = excluded.api_key, default_model = excluded.default_model, enabled = excluded.enabled,
           extra_headers = excluded.extra_headers, metadata = excluded.metadata`,
        {
          $id: config.id,
          $label: config.label,
          $kind: config.kind,
          $baseUrl: config.baseUrl,
          $apiKey: config.apiKey ?? null,
          $defaultModel: config.defaultModel,
          $enabled: toIntBool(config.enabled),
          $extraHeaders:
            config.extraHeaders === undefined
              ? null
              : toJson(config.extraHeaders),
          $metadata:
            config.metadata === undefined ? null : toJson(config.metadata),
        },
      );
    }, this.txOwner);
    return config;
  }

  async deleteProvider(providerId: string): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(`DELETE FROM providers WHERE id = $id`, {
        $id: providerId,
      });
      this.conn.run(`DELETE FROM provider_models WHERE provider_id = $id`, {
        $id: providerId,
      });
      this.conn.run(
        `DELETE FROM provider_capabilities WHERE provider_id = $id`,
        {
          $id: providerId,
        },
      );
    }, this.txOwner);
  }

  async listModels(providerId: string): Promise<AiProviderModel[]> {
    const rows = this.conn.all(
      `SELECT * FROM provider_models WHERE provider_id = $id ORDER BY model_id ASC`,
      { $id: providerId },
    ) as ProviderModelRow[];
    return rows.map(modelFromRow);
  }

  async replaceModels(
    providerId: string,
    models: AiProviderModel[],
  ): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(`DELETE FROM provider_models WHERE provider_id = $id`, {
        $id: providerId,
      });
      for (const model of models) {
        this.conn.run(
          `INSERT INTO provider_models
             (provider_id, model_id, display_name, context_window_tokens, supports_tool_calling, fetched_at)
           VALUES ($providerId, $modelId, $displayName, $contextWindowTokens, $supportsToolCalling, $fetchedAt)`,
          {
            $providerId: providerId,
            $modelId: model.modelId,
            $displayName: model.displayName,
            $contextWindowTokens: model.contextWindowTokens ?? null,
            $supportsToolCalling: toOptionalIntBool(model.supportsToolCalling),
            $fetchedAt: model.fetchedAt,
          },
        );
      }
    }, this.txOwner);
  }

  async getCapabilities(
    providerId: string,
  ): Promise<AiProviderCapabilities | null> {
    const row = this.conn.get(
      `SELECT * FROM provider_capabilities WHERE provider_id = $id`,
      { $id: providerId },
    ) as ProviderCapabilitiesRow | undefined;
    return row ? capabilitiesFromRow(row) : null;
  }

  async saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO provider_capabilities
           (provider_id, streaming, tool_calling, model_list, vision, json_mode, max_context_tokens, checked_at, warning)
         VALUES ($id, $streaming, $toolCalling, $modelList, $vision, $jsonMode, $maxContextTokens, $checkedAt, $warning)
         ON CONFLICT(provider_id) DO UPDATE SET
           streaming = excluded.streaming, tool_calling = excluded.tool_calling,
           model_list = excluded.model_list, vision = excluded.vision, json_mode = excluded.json_mode,
           max_context_tokens = excluded.max_context_tokens, checked_at = excluded.checked_at, warning = excluded.warning`,
        {
          $id: providerId,
          $streaming: toIntBool(capabilities.streaming),
          $toolCalling: toIntBool(capabilities.toolCalling),
          $modelList: toIntBool(capabilities.modelList),
          $vision: toOptionalIntBool(capabilities.vision),
          $jsonMode: toOptionalIntBool(capabilities.jsonMode),
          $maxContextTokens: capabilities.maxContextTokens ?? null,
          $checkedAt: capabilities.checkedAt ?? null,
          $warning: capabilities.warning ?? null,
        },
      );
    }, this.txOwner);
  }
}
