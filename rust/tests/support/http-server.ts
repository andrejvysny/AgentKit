import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createAgentKitClient } from "@agentkit/client";

const root = resolve(import.meta.dir, "../../..");
let build: Promise<void> | undefined;

/**
 * Native interop is qualified by the Rust workflow, where the toolchain is
 * set up and the fixture is built from a fetched dependency cache. The plain
 * TypeScript `bun test` must not conflate the two: it skips native-backed
 * tests unless the environment explicitly asks for them.
 */
export function nativeInteropRequested(): boolean {
  return (
    process.env["AGENTKIT_NATIVE_FIXTURE"] !== undefined ||
    process.env["AGENTKIT_TEST_BACKEND"] === "rust" ||
    process.env["AGENTKIT_RUN_RUST_CONFORMANCE"] === "1" ||
    process.env["AGENTKIT_RUN_RUST_RUNNER_CONFORMANCE"] === "1" ||
    process.env["AGENTKIT_RUN_RUST_INTEROP"] === "1"
  );
}

/** Defined when native tests should skip, naming how to enable them. */
export const nativeFixtureSkipReason: string | undefined =
  nativeInteropRequested()
    ? undefined
    : "native interop belongs to the Rust workflow; run it there, or set AGENTKIT_RUN_RUST_INTEROP=1 / AGENTKIT_NATIVE_FIXTURE=<binary> to qualify it here";

async function runCargo(
  args: string[],
): Promise<{ code: number; stderr: string }> {
  const child = Bun.spawn(["cargo", ...args], {
    cwd: root,
    stdout: "ignore",
    stderr: "pipe",
  });
  const stderr = await new Response(child.stderr).text();
  return { code: await child.exited, stderr };
}

function infraDiagnosis(detail: string): string {
  return (
    `Native HTTP fixture preparation failed — this is a test-infrastructure ` +
    `problem, not a product failure. ${detail}\n` +
    `Preparation steps: (1) resolve dependencies with ` +
    `\`cargo fetch --manifest-path rust/Cargo.toml --locked\` during a ` +
    `network-permitted setup phase, (2) build with \`cargo build ` +
    `--manifest-path rust/Cargo.toml -p agentkit-host --features http ` +
    `--example http_fixture --locked\`, or (3) point ` +
    `AGENTKIT_NATIVE_FIXTURE at a prebuilt rust/target/debug/examples/http_fixture. ` +
    `Set AGENTKIT_CARGO_OFFLINE=1 only once dependency availability is ` +
    `established (the Rust workflow does this after its cargo test step).`
  );
}

async function prepareFixture(): Promise<void> {
  const manifest = ["--manifest-path", "rust/Cargo.toml"];
  const buildArgs = [
    "build",
    ...manifest,
    "-p",
    "agentkit-host",
    "--features",
    "http",
    "--example",
    "http_fixture",
    "--locked",
  ];
  if (process.env["AGENTKIT_CARGO_OFFLINE"] === "1") {
    // The caller asserts the dependency cache is complete.
    const offline = await runCargo([...buildArgs, "--offline"]);
    if (offline.code !== 0)
      throw new Error(
        infraDiagnosis(
          `offline build (AGENTKIT_CARGO_OFFLINE=1):\n${offline.stderr}`,
        ),
      );
    return;
  }
  // Setup phase: resolve dependencies while the network is permitted.
  const fetched = await runCargo(["fetch", ...manifest, "--locked"]);
  if (fetched.code === 0) {
    // Availability established — build against the local cache.
    const offline = await runCargo([...buildArgs, "--offline"]);
    if (offline.code === 0) return;
    // A cache the resolver disagrees with is an infrastructure problem, but
    // one online retry distinguishes "cache incomplete" from "build broken".
    const online = await runCargo(buildArgs);
    if (online.code === 0) return;
    throw new Error(
      infraDiagnosis(
        `build failed after a successful fetch:\noffline:\n${offline.stderr}\nonline:\n${online.stderr}`,
      ),
    );
  }
  // No network (or fetch otherwise failed): the cache may still be complete.
  const offline = await runCargo([...buildArgs, "--offline"]);
  if (offline.code === 0) return;
  throw new Error(
    infraDiagnosis(
      `both dependency resolution and the offline build failed:\nfetch:\n${fetched.stderr}\noffline build:\n${offline.stderr}`,
    ),
  );
}

async function ensureBinary(): Promise<string> {
  const override = process.env["AGENTKIT_NATIVE_FIXTURE"];
  if (override !== undefined) return override;
  build ??= prepareFixture();
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
