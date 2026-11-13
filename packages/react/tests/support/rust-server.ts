import type { TestProposalService } from "../../../client/tests/support/test-types.js";
import { startRustClientServer } from "../../../client/tests/support/rust-server.js";
import type { TestServer, TestServerOptions } from "./server.js";

/** Portable React scenarios use the same native SQLite backend as the client suite. */
export async function startRustReactServer(
  options: TestServerOptions,
): Promise<TestServer> {
  const native = await startRustClientServer({
    ...(options.contributors === undefined
      ? {}
      : { contributors: options.contributors }),
    ...(options.provider === undefined ? {} : { provider: options.provider }),
    ...(options.deps === undefined ? {} : { deps: options.deps }),
  });
  if (options.provider === undefined)
    (
      native.provider as import("@agentkit/testing").MockProviderClient
    ).setScript([{ steps: [{ kind: "text", content: "Hello, hooks." }] }]);
  const proposals = new Proxy(Object.create(null) as TestProposalService, {
    get(_target, key) {
      if (key === "stage")
        return async (input: Parameters<TestProposalService["stage"]>[0]) =>
          native.control("/proposal", input) as ReturnType<
            TestProposalService["stage"]
          >;
      throw new Error(
        `Native proposal test control does not implement ${String(key)}.`,
      );
    },
  });
  return {
    baseUrl: native.baseUrl,
    provider: native.provider,
    get store() {
      return native.store;
    },
    get secrets() {
      return native.secrets;
    },
    proposals,
    stop: () => native.stop(),
  };
}
