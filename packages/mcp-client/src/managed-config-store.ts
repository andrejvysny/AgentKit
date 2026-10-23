import type { SecretStore } from "@agentkit/host";
import type {
  McpServerConfigStore,
  McpServerConfigRecord,
  McpServerConfigPatch,
} from "./config-store.js";
import { normalizeConnectionSecrets } from "./connections.js";
import type { McpClientManager } from "./manager.js";

/** Wire this decorator into existing REST mcpConfigs to apply every edit live. */
export class ManagedMcpConfigStore implements McpServerConfigStore {
  private queue: Promise<unknown> = Promise.resolve();
  constructor(
    private readonly store: McpServerConfigStore,
    private readonly manager: McpClientManager,
    private readonly secrets: SecretStore,
  ) {}
  private serialize<T>(fn: () => Promise<T>): Promise<T> {
    const next = this.queue.then(fn);
    this.queue = next.catch(() => {});
    return next;
  }
  list() {
    return this.store.list();
  }
  get(id: string) {
    return this.store.get(id);
  }
  create(record: McpServerConfigRecord) {
    return this.serialize(async () => {
      const config = await normalizeConnectionSecrets(
        record.id,
        record,
        this.secrets,
      );
      const created = await this.store.create({ ...record, ...config });
      await this.manager.replace(created);
      return created;
    });
  }
  update(id: string, patch: McpServerConfigPatch) {
    return this.serialize(async () => {
      const old = await this.store.get(id);
      if (!old) return this.store.update(id, patch);
      const next = await normalizeConnectionSecrets(
        id,
        { ...old, ...patch },
        this.secrets,
        old,
      );
      const updated = await this.store.update(id, next);
      if (old.alias !== updated.alias) await this.manager.remove(old.alias);
      await this.manager.replace(updated);
      return updated;
    });
  }
  delete(id: string) {
    return this.serialize(async () => {
      const old = await this.store.get(id);
      await this.store.delete(id);
      if (old) await this.manager.remove(old.alias);
    });
  }
  /** Startup import, including edits performed by another process. */
  reconcile() {
    return this.serialize(async () => {
      const records = await this.store.list();
      for (const alias of this.manager.aliases())
        if (!records.some((record) => record.alias === alias))
          await this.manager.remove(alias);
      for (const record of records) await this.manager.replace(record);
    });
  }
}
