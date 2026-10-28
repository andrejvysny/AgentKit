import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createAgentKitClient } from "@agentkit/client";

const root = resolve(import.meta.dir, "../../..");
let build: Promise<void> | undefined;

// Cross-language tests need a compiled native fixture. The intentional job for
// them is the `native` job in .github/workflows/rust.yml: pinned toolchain,
// locked dependency fetch, warm cargo cache, one prebuilt fixture. Ordinary
// TypeScript CI (`.github/workflows/ci.yml`) sets AGENTKIT_SKIP_NATIVE=1
// because it has no prepared Cargo cache — an offline `cargo build` there
// fails on missing registry packages and every native test would fail for a
// test-setup reason, not a product reason. Native tests must gate on this
// flag with test.skipIf/describe.skipIf, and the jobs that run them must not
// set the variable.
export const nativeBackendAvailable =
  process.env["AGENTKIT_SKIP_NATIVE"] !== "1";

async function ensureBinary(): Promise<string> {
  if (!nativeBackendAvailable)
    throw new Error(
      "Native fixture disabled via AGENTKIT_SKIP_NATIVE=1; gate the test with nativeBackendAvailable instead of reaching the fixture.",
    );
  const override = process.env["AGENTKIT_NATIVE_FIXTURE"];
  if (override !== undefined) return override;
  build ??= (async () => {
    const child = Bun.spawn(
      [
        "cargo",
        "build",
        "--manifest-path",
        "rust/Cargo.toml",
        "-p",
        "agentkit-host",
        "--features",
        "http",
        "--example",
        "http_fixture",
        "--locked",
      ],
      {
        cwd: root,
        stdout: "ignore",
        stderr: "pipe",
      },
    );
    const errors = await new Response(child.stderr).text();
    if ((await child.exited) !== 0)
      throw new Error(`Native HTTP fixture build failed: ${errors}`);
  })();
  await build;
  return join(root, "rust/target/debug/examples/http_fixture");
}

export interface NativeServer {
  baseUrl: string;
  token: string;
  controlUrl: string;
  controlToken: string;
  client: ReturnType<typeof createAgentKitClient>;
  restart(): Promise<NativeServer>;
  stop(): Promise<void>;
}

export async function startNativeServer(existing?: {
  directory?: string;
  token?: string;
  compatibility?: boolean;
  providerUrl?: string;
  providerToken?: string;
  contributorNamespaces?: string[];
}): Promise<NativeServer> {
  const binary = await ensureBinary();
  const directory =
    existing?.directory ??
    (await mkdtemp(join(tmpdir(), "agentkit-native-http-")));
  const token = existing?.token ?? `fixture-${crypto.randomUUID()}`;
  const compatibility = existing?.compatibility ?? false;
  const controlToken = `control-${crypto.randomUUID()}`;
  const child = Bun.spawn([binary, join(directory, "store.sqlite")], {
    env: {
      ...process.env,
      AGENTKIT_TEST_TOKEN: token,
      AGENTKIT_TEST_CONTROL_TOKEN: controlToken,
      AGENTKIT_TEST_COMPAT: compatibility ? "1" : "0",
      ...(existing?.contributorNamespaces === undefined
        ? {}
        : {
            AGENTKIT_TEST_CONTRIBUTORS: JSON.stringify(
              existing.contributorNamespaces,
            ),
          }),
      ...(existing?.providerUrl === undefined
        ? {}
        : {
            AGENTKIT_TEST_PROVIDER_URL: existing.providerUrl,
            AGENTKIT_TEST_PROVIDER_TOKEN: existing.providerToken ?? "",
          }),
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const reader = child.stdout.getReader();
  let output = "";
  while (!output.includes("\n")) {
    const chunk = await reader.read();
    if (chunk.done)
      throw new Error(
        `Native fixture startup failed: ${await new Response(child.stderr).text()}`,
      );
    output += new TextDecoder().decode(chunk.value);
  }
  reader.releaseLock();
  const announced = JSON.parse(output.split("\n")[0]!) as {
    baseUrl: string;
    controlUrl: string;
  };
  const { baseUrl, controlUrl } = announced;
  const client = createAgentKitClient({
    baseUrl,
    headers: () => ({ authorization: `Bearer ${token}` }),
  });
  const close = async (): Promise<void> => {
    child.kill();
    await child.exited;
  };
  return {
    baseUrl,
    token,
    controlUrl,
    controlToken,
    client,
    async restart() {
      await close();
      return startNativeServer({
        ...existing,
        directory,
        token,
        compatibility,
      });
    },
    async stop() {
      await close();
      await rm(directory, { recursive: true, force: true });
    },
  };
}

export type CompatibilityBackend = "typescript" | "rust";
export interface CompatibilityServer {
  client: ReturnType<typeof createAgentKitClient>;
  stop(): Promise<void>;
}

export async function startCompatibilityServer(
  backend: CompatibilityBackend,
): Promise<CompatibilityServer> {
  if (backend === "rust") return startNativeServer();
  const { startTestServer } = await import(
    "../../../packages/client/tests/support/server.js"
  );
  const server = await startTestServer({ backend: "typescript" });
  return {
    client: createAgentKitClient({ baseUrl: server.baseUrl }),
    stop: () => server.stop(),
  };
}
