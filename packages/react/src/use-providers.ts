/**
 * `useProviders` — the settings screen's data, and nothing more clever.
 *
 * Every action here is a straight passthrough to `@agentkit/client` wrapped in
 * busy/error state and followed by a re-read. There is no optimistic update on
 * purpose: a provider row is edited once in a while by one person looking at
 * the form, so the round trip costs nothing a user notices, and a config that
 * briefly showed a value the server rejected is worse than a form that took
 * 80ms.
 *
 * `apiKey` IS NEVER IN THIS STATE, and cannot be: it is write-only in the
 * contract — the server hands it to a `SecretStore` under a ref and publishes
 * the ref — so a `ProviderDto` has no field to carry it back in. The one place
 * a key exists client-side is the argument to {@link UseProvidersResult.create}
 * or `update`, which this hook forwards and does not keep. The same is true of
 * `extraHeaders`, where a gateway token routinely lives.
 */
import type { AgentKitClient, AgentKitClientError } from "@agentkit/client";
import type {
  CreateProviderRequest,
  ModelDto,
  ProviderDto,
  TestProviderResponse,
  UpdateProviderRequest,
} from "@agentkit/contracts";
import { useCallback, useEffect } from "react";
import { useAgentKitClient } from "./context.js";
import { isAbort, toError, useAliveRef, useMirroredState } from "./internal.js";

export interface ProvidersState {
  providers: ProviderDto[];
  /** Catalogues keyed by provider id, for the ones that have been read. */
  models: Record<string, ModelDto[]>;
  loading: boolean;
  /** A write (create/update/delete/refresh/test) is in flight. */
  busy: boolean;
  error: AgentKitClientError | Error | null;
}

export interface UseProvidersOptions {
  client?: AgentKitClient;
}

export interface UseProvidersResult extends ProvidersState {
  create(body: CreateProviderRequest): Promise<ProviderDto | null>;
  update(
    providerId: string,
    body: UpdateProviderRequest,
  ): Promise<ProviderDto | null>;
  /** `delete` is a reserved word; this is `client.deleteProvider`. */
  remove(providerId: string): Promise<boolean>;
  /** Read the stored catalogue into {@link ProvidersState.models}. */
  loadModels(providerId: string): Promise<ModelDto[] | null>;
  /** Re-probe the endpoint and REPLACE the stored catalogue. A write. */
  refreshModels(providerId: string): Promise<ModelDto[] | null>;
  /**
   * Probe reachability and credentials. `{ ok: false }` is a successful call
   * reporting a failed probe, so it lands in the return value, not in `error`.
   */
  test(providerId: string): Promise<TestProviderResponse | null>;
  reload(): Promise<void>;
}

const EMPTY: ProvidersState = {
  providers: [],
  models: {},
  loading: false,
  busy: false,
  error: null,
};

export function useProviders(
  options: UseProvidersOptions = {},
): UseProvidersResult {
  const client = useAgentKitClient(options.client);
  const alive = useAliveRef();
  const { value, update } = useMirroredState<ProvidersState>(EMPTY);

  const refresh = useCallback(
    async (signal?: AbortSignal): Promise<void> => {
      update((prev) => ({ ...prev, loading: true }));
      try {
        const providers = await client.listProviders(
          signal === undefined ? undefined : { signal },
        );
        if (signal?.aborted === true || !alive.current) return;
        update((prev) => ({ ...prev, providers, loading: false, error: null }));
      } catch (cause) {
        if (isAbort(cause, signal) || !alive.current) return;
        update((prev) => ({ ...prev, loading: false, error: toError(cause) }));
      }
    },
    [client, update, alive],
  );

  /** One write, with busy/error bookkeeping and a `null` on failure. */
  const run = useCallback(
    async <T>(call: () => Promise<T>): Promise<T | null> => {
      update((prev) => ({ ...prev, busy: true, error: null }));
      try {
        const result = await call();
        if (alive.current) update((prev) => ({ ...prev, busy: false }));
        return result;
      } catch (cause) {
        if (!alive.current) return null;
        update((prev) => ({ ...prev, busy: false, error: toError(cause) }));
        return null;
      }
    },
    [update, alive],
  );

  const create = useCallback<UseProvidersResult["create"]>(
    async (body) => {
      const provider = await run(() => client.createProvider(body));
      if (provider !== null) await refresh();
      return provider;
    },
    [run, client, refresh],
  );

  const updateProvider = useCallback<UseProvidersResult["update"]>(
    async (providerId, body) => {
      const provider = await run(() =>
        client.updateProvider({ providerId }, body),
      );
      if (provider !== null) await refresh();
      return provider;
    },
    [run, client, refresh],
  );

  const remove = useCallback<UseProvidersResult["remove"]>(
    async (providerId) => {
      const done = await run(async () => {
        await client.deleteProvider({ providerId });
        return true;
      });
      if (done === true) {
        update((prev) => {
          const { [providerId]: _dropped, ...models } = prev.models;
          return { ...prev, models };
        });
        await refresh();
      }
      return done === true;
    },
    [run, client, refresh, update],
  );

  const rememberModels = useCallback(
    (providerId: string, models: ModelDto[] | null): ModelDto[] | null => {
      if (models === null) return null;
      update((prev) => ({
        ...prev,
        models: { ...prev.models, [providerId]: models },
      }));
      return models;
    },
    [update],
  );

  const loadModels = useCallback<UseProvidersResult["loadModels"]>(
    async (providerId) =>
      rememberModels(
        providerId,
        await run(() => client.listModels({ providerId })),
      ),
    [run, client, rememberModels],
  );

  const refreshModels = useCallback<UseProvidersResult["refreshModels"]>(
    async (providerId) =>
      rememberModels(
        providerId,
        await run(() => client.refreshProviderModels({ providerId })),
      ),
    [run, client, rememberModels],
  );

  const test = useCallback<UseProvidersResult["test"]>(
    (providerId) => run(() => client.testProvider({ providerId })),
    [run, client],
  );

  useEffect(() => {
    const controller = new AbortController();
    void refresh(controller.signal);
    return () => controller.abort();
  }, [refresh]);

  const reload = useCallback<UseProvidersResult["reload"]>(
    () => refresh(),
    [refresh],
  );

  return {
    ...value,
    create,
    update: updateProvider,
    remove,
    loadModels,
    refreshModels,
    test,
    reload,
  };
}
