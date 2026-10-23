import type { AiProviderClient } from "@agentkit/core";
import { startProviderBridge } from "./provider-bridge.js";
import type {
  MemoryAssistantStore,
  MemorySecretStore,
} from "@agentkit/adapters-memory";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import { startNativeServer } from "../../../../rust/tests/support/http-server.js";
import type { TestServer, TestServerOptions } from "./server.js";

export interface NativeClientServer<
  T extends AiProviderClient = MockProviderClient,
> extends Omit<TestServer, "provider"> {
  provider: T;
  control(path: string, body: unknown): Promise<unknown>;
}

type NativeOptions = Omit<TestServerOptions, "provider"> & {
  provider?: AiProviderClient;
  contributors?: import("@agentkit/host").ToolSetContributor[];
};
export function startRustClientServer(
  options: TestServerOptions,
): Promise<NativeClientServer>;
export function startRustClientServer(
  options: NativeOptions,
): Promise<NativeClientServer<AiProviderClient>>;
/** Native state stays in SQLite; provider callbacks cross a private test-only socket. */
export async function startRustClientServer(
  options: NativeOptions,
): Promise<NativeClientServer<AiProviderClient>> {
  if (Object.keys(options.deps ?? {}).some((key) => key !== "authenticate"))
    throw new Error(
      "Native fixture supports only the authenticate test gateway override.",
    );
  const bridge =
    options.provider === undefined
      ? undefined
      : startProviderBridge(options.provider, options.contributors);
  let native: Awaited<ReturnType<typeof startNativeServer>>;
  try {
    native = await startNativeServer({
      compatibility: true,
      ...(options.contributors === undefined
        ? {}
        : {
            contributorNamespaces: options.contributors.map(
              (contributor) => contributor.namespace,
            ),
          }),
      ...(bridge === undefined
        ? {}
        : { providerUrl: bridge.url, providerToken: bridge.token }),
    });
  } catch (error) {
    await bridge?.stop();
    throw error;
  }
  let pending = Promise.resolve();
  const sendScript = (turns: MockTurn[]): void => {
    pending = pending.then(async () => {
      const response = await fetch(`${native.controlUrl}/script`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${native.controlToken}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(turns),
      });
      if (!response.ok)
        throw new Error(
          `Native script control rejected input: ${response.status}`,
        );
    });
    void pending.catch(() => undefined);
  };
  class RemoteProvider extends MockProviderClient {
    override setScript(turns: MockTurn[]): void {
      sendScript(turns);
    }
    override streamChat(): AsyncGenerator<never> {
      throw new Error(
        "Direct native provider iteration requires the native callback bridge.",
      );
    }
  }
  const provider = options.provider ?? new RemoteProvider();
  const control = async (path: string, body: unknown): Promise<unknown> => {
    await pending;
    const response = await fetch(`${native.controlUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${native.controlToken}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    if (!response.ok)
      throw new Error(`Native test control failed: ${response.status}`);
    return response.json();
  };
  let secretValues = new Map<string, string>();
  const secrets = new Proxy(Object.create(null) as MemorySecretStore, {
    get(_target, key) {
      if (key === "values") return new Map(secretValues);
      if (key === "get")
        return async (ref: string) => control("/secret", { ref });
      throw new Error(
        `Native SecretStore test control does not implement ${String(key)}.`,
      );
    },
  });
  type StoredTask = {
    taskId: string;
    scopeId: string;
    status: string;
  } & Record<string, unknown>;
  let finalTasks: StoredTask[] | undefined;
  let stopping: Promise<void> | undefined;
  const leaseProofs = new Map<string, unknown>();
  const taskMethods = {
    getTask: (taskId: string) =>
      finalTasks === undefined
        ? control("/tasks", { operation: "getTask", taskId })
        : Promise.resolve(
            finalTasks.find((task) => task.taskId === taskId) ?? null,
          ),
    listByScope: (scopeId: string) =>
      finalTasks === undefined
        ? control("/tasks", { operation: "listByScope", scopeId })
        : Promise.resolve(
            finalTasks.filter((task) => task.scopeId === scopeId),
          ),
    listEvents: (taskId: string, options?: unknown) =>
      control("/tasks", { operation: "listEvents", taskId, options }),
    async acquireLease(input: unknown) {
      const lease = (await control("/tasks", {
        operation: "acquireLease",
        input,
      })) as { leaseToken: string };
      leaseProofs.set(lease.leaseToken, lease);
      return lease;
    },
    appendEvents(
      taskId: string,
      events: unknown,
      options: { leaseToken: string },
    ) {
      const proof = leaseProofs.get(options.leaseToken);
      if (proof === undefined)
        throw new Error("Native append requires a real acquired lease proof.");
      return control("/tasks", {
        operation: "appendEvents",
        taskId,
        events,
        proof,
      });
    },
    nextSeq: (taskId: string) =>
      control("/tasks", { operation: "nextSeq", taskId }),
  };
  const tasks = new Proxy(taskMethods, {
    get(target, key) {
      if (key in target) return Reflect.get(target, key);
      throw new Error(
        `Native TaskStore test control does not implement ${String(key)}.`,
      );
    },
  });
  bridge?.setEventsReader((runId, options) => tasks.listEvents(runId, options));
  const store = new Proxy(Object.create(null) as MemoryAssistantStore, {
    get(_target, key) {
      if (key === "tasks") return tasks;
      throw new Error(
        `Native AssistantStore test control does not implement ${String(key)}.`,
      );
    },
  });
  const gateway = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    idleTimeout: 0,
    async fetch(request) {
      await pending;
      if (options.deps?.authenticate !== undefined) {
        const authenticationRequest = new Request(request.url, {
          method: request.method,
          headers: request.headers,
          ...(request.body === null
            ? {}
            : { body: await request.clone().arrayBuffer() }),
        });
        const principal = await options.deps.authenticate(
          authenticationRequest,
        );
        if (principal instanceof Response) return principal;
      }
      const url = new URL(request.url);
      const headers = new Headers(request.headers);
      headers.set("authorization", `Bearer ${native.token}`);
      const upstream = await fetch(
        `${native.baseUrl}${url.pathname}${url.search}`,
        {
          method: request.method,
          headers,
          ...(request.body === null
            ? {}
            : { body: await request.arrayBuffer() }),
          signal: request.signal,
        },
      );
      if (
        upstream.ok &&
        url.pathname.startsWith("/v1/providers") &&
        ["POST", "PATCH", "DELETE"].includes(request.method)
      ) {
        secretValues = new Map(
          Object.entries(
            (await control("/secrets", {})) as Record<string, string>,
          ),
        );
      }
      return new Response(upstream.body, {
        status: upstream.status,
        headers: upstream.headers,
      });
    },
  });
  return {
    baseUrl: `http://127.0.0.1:${gateway.port}`,
    provider,
    control,
    store,
    secrets,
    stop() {
      stopping ??= (async () => {
        try {
          await pending;
          const tasks = (await control("/tasks", {
            operation: "listByScope",
          })) as StoredTask[];
          for (const task of tasks) {
            if (!["completed", "failed", "cancelled"].includes(task.status))
              await native.client.cancelRun({ runId: task.taskId });
          }
          const deadline = Date.now() + 5000;
          do {
            finalTasks = (await control("/tasks", {
              operation: "listByScope",
            })) as StoredTask[];
            if (
              finalTasks.every((task) =>
                ["completed", "failed", "cancelled"].includes(task.status),
              )
            )
              break;
            await new Promise((resolve) => setTimeout(resolve, 5));
          } while (Date.now() < deadline);
        } finally {
          await gateway.stop(true);
          await bridge?.stop();
          await native.stop();
        }
      })();
      return stopping;
    },
  };
}
