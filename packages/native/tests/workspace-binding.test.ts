import { testServiceOptions } from "./helpers/test-profiles.js";
/**
 * Workspace binding of the native coding lane through the PUBLIC binding:
 * tools registered for workspace A are offered/executable only to sessions
 * whose descriptor workspace is A. A session naming workspace B gets no
 * coding tools (and so cannot hand them to subagent children, which copy
 * the parent's descriptor workspace — pinned in the host's
 * `child_descriptor_carries_the_parent_workspace` test). Sessions without a
 * workspace keep the service-wide behavior unless registration is strict.
 */
import { expect, test } from "bun:test";
import { mkdtempSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeSessionService, NativeStore } from "../src/index.js";
import { available } from "./helpers/binding.js";
import {
  startProviderMock,
  type CapturedRequest,
  type LoopbackOptions,
} from "./helpers/loopback-provider.js";

const nativeTest = available ? test : test.skip;
const SECRET = "workspace-a-secret-contents";

function dirWith(name: string, contents: string): string {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), "agentkit-ws-binding-")));
  writeFileSync(join(dir, name), contents);
  return dir;
}

async function open(options: {
  root: string;
  strict?: boolean;
  stages: NonNullable<LoopbackOptions["chatStages"]>;
}) {
  const provider = await startProviderMock({ chatStages: options.stages });
  const dir = mkdtempSync(join(tmpdir(), "agentkit-ws-binding-store-"));
  const store = await NativeStore.open(join(dir, "store.sqlite"));
  const connectionId = await NativeSessionService.registerProviderConnection(
    provider.baseUrl,
    null,
  );
  const service = await store.createService(
    testServiceOptions({
      jsToolAuthority: { configuredMode: "full_auto" },
      providerConfig: {
        protocol: "chat-completions",
        providerId: "probe",
        connectionId,
        model: "loopback-model",
      },
    }),
  );
  await service.registerCodingTools({
    workspaceRoot: options.root,
    sandbox: "off",
    configuredMode: "full_auto",
    ...(options.strict ? { requireSessionWorkspace: true } : {}),
  });
  const close = async () => {
    await service.close().catch(() => undefined);
    await store.close();
    await provider.stop();
  };
  return { provider, service, close };
}

async function runTurn(
  service: NativeSessionService,
  workspaceRoot: string | undefined,
): Promise<void> {
  const session = (await service.createSession({
    profileId: "coding",
    ...(workspaceRoot === undefined
      ? {}
      : { workspace: { root: workspaceRoot, trusted: true } }),
  })) as Record<string, unknown>;
  const sessionId = String(session.id ?? session.sessionId);
  const sent = (await service.send(sessionId, {
    content: "read it",
  })) as Record<string, unknown>;
  const turnId = String(sent.turnId ?? "");
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    const turns = (await service.listTurns(sessionId)) as {
      id?: string;
      turnId?: string;
      status?: string;
    }[];
    const status = turns.find(
      (t) => t.id === turnId || t.turnId === turnId,
    )?.status;
    if (status === "completed" || status === "failed") return;
    await Bun.sleep(25);
  }
  throw new Error("turn did not settle");
}

function toolNames(request: CapturedRequest): string[] {
  const tools = (request.body.tools ?? []) as {
    function?: { name?: string };
  }[];
  return tools.map((tool) => tool.function?.name ?? "");
}

const readCall = {
  toolCalls: [{ name: "read_file", arguments: { path: "secret.txt" } }],
};

nativeTest(
  "a session of another workspace gets no coding tools on the same service",
  async () => {
    const a = dirWith("secret.txt", SECRET);
    const b = dirWith("other.txt", "b");
    const { provider, service, close } = await open({
      root: a,
      stages: [readCall, { deltas: ["done"] }, readCall, { deltas: ["done"] }],
    });
    try {
      await runTurn(service, a);
      expect(toolNames(provider.requests[0]!)).toContain("read_file");
      expect(JSON.stringify(provider.requests[1]!.body.messages)).toContain(
        SECRET,
      );
      const before = provider.requests.length;
      await runTurn(service, b);
      const bRequests = provider.requests.slice(before);
      expect(bRequests.length).toBeGreaterThan(0);
      for (const request of bRequests) {
        expect(toolNames(request)).not.toContain("read_file");
        expect(JSON.stringify(request.body.messages)).not.toContain(SECRET);
      }
    } finally {
      await close();
    }
  },
  { timeout: 90_000 },
);

nativeTest(
  "strict registration withholds coding tools from sessions without a workspace",
  async () => {
    const a = dirWith("secret.txt", SECRET);
    const { provider, service, close } = await open({
      root: a,
      strict: true,
      stages: [{ deltas: ["done"] }],
    });
    try {
      await runTurn(service, undefined);
      expect(toolNames(provider.requests[0]!)).not.toContain("read_file");
      const before = provider.requests.length;
      await runTurn(service, a);
      expect(toolNames(provider.requests[before]!)).toContain("read_file");
    } finally {
      await close();
    }
  },
  { timeout: 90_000 },
);

nativeTest(
  "non-strict registration keeps the service-wide tools for sessions without a workspace",
  async () => {
    const a = dirWith("secret.txt", SECRET);
    const { provider, service, close } = await open({
      root: a,
      stages: [{ deltas: ["done"] }],
    });
    try {
      await runTurn(service, undefined);
      expect(toolNames(provider.requests[0]!)).toContain("read_file");
    } finally {
      await close();
    }
  },
  { timeout: 90_000 },
);
