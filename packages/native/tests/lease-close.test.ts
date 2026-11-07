import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeAgentError, NativeStore } from "../src/index.js";
import { testProfile } from "./helpers/profiles.js";

interface Receipt {
  status: string;
  result?: { ok: boolean; data?: { errorCode?: string; started?: boolean } };
}

async function receipt(
  store: NativeStore,
  runId: string,
): Promise<Receipt | null> {
  const records = await store.callStore<Receipt[]>("toolExecutions.listByRun", {
    runId,
  });
  return records[0] ?? null;
}

async function waitForAdmission(
  store: NativeStore,
  runId: string,
  locks: string,
): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (
    (await receipt(store, runId))?.status !== "admitted" ||
    !readdirSync(locks).some((name) => name.startsWith("intent-"))
  ) {
    if (Date.now() > deadline)
      throw new Error("native command never waited for external lease");
    await Bun.sleep(10);
  }
}

async function windowsCommandRefusal(): Promise<void> {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-windows-command-"));
  const path = join(directory, "store.sqlite");
  const store = await NativeStore.open(path);
  const service = await store.createService({
    jsToolAuthority: { configuredMode: "full_auto" },
    // The refusal scenario still has to stage the native command tool it
    // expects the pack to refuse; full_auto so nothing else gates it.
    profiles: [
      testProfile("windows-refusal", {
        tools: ["run_command"],
        permissionMode: "full_auto",
      }),
    ],
  });
  try {
    try {
      await service.registerCodingTools({
        workspaceRoot: directory,
        sandbox: "off",
        configuredMode: "full_auto",
      });
    } catch (error) {
      expect(error).toBeInstanceOf(NativeAgentError);
      expect((error as NativeAgentError).code).toBe("invalid_input");
      expect((error as Error).message).toContain(
        "workspace.root must be an absolute path",
      );
      expect(existsSync(join(directory, "effect"))).toBe(false);
      return;
    }
    let calls = 0;
    service.setProvider(async () => {
      calls += 1;
      return calls === 1
        ? {
            content: "",
            finishReason: "tool_calls",
            toolCalls: [
              {
                id: "unsupported-command",
                name: "run_command",
                arguments: { command: "echo effect > effect" },
              },
            ],
          }
        : { content: "done", finishReason: "stop", toolCalls: [] };
    });
    const session = await service.createSession({
      profileId: "windows-refusal",
      model: "test",
      executionMode: "full_auto",
    });
    const sent = await service.send(session.sessionId as string, {
      content: { type: "text", text: "run" },
    });
    if (sent.kind !== "turn") throw new Error("expected native command turn");
    const deadline = Date.now() + 5_000;
    while ((await receipt(store, sent.turnId))?.status !== "completed") {
      if (Date.now() > deadline)
        throw new Error("unsupported command did not refuse");
      await Bun.sleep(10);
    }
    const refusal = await receipt(store, sent.turnId);
    expect(refusal?.result?.ok).toBe(false);
    expect(JSON.stringify(refusal?.result)).toContain(
      "not supported on this platform",
    );
    expect(existsSync(join(directory, "effect"))).toBe(false);
  } finally {
    await service.close();
    await store.close();
    rmSync(directory, { recursive: true, force: true });
  }
}

test("native close drains no-start receipt while external workspace lease stays held", async () => {
  if (process.platform === "win32") return windowsCommandRefusal();
  const directory = realpathSync(
    mkdtempSync(join(tmpdir(), "agentkit-lease-close-")),
  );
  const workspace = join(directory, "workspace");
  const coordination = join(directory, "coordination");
  mkdirSync(workspace, { mode: 0o700 });
  mkdirSync(coordination, { mode: 0o700 });
  const locks = join(
    coordination,
    createHash("sha256").update(workspace).digest("hex"),
  );
  mkdirSync(locks, { mode: 0o700 });
  const lock = join(locks, "exclusive.lock");
  const owner = JSON.stringify({
    pid: process.pid,
    tool: "external-test-holder",
    label: "held",
  });
  writeFileSync(lock, owner, { mode: 0o600 });
  const path = join(directory, "store.sqlite");
  const store = await NativeStore.open(path);
  const service = await store.createService({
    closeTimeoutMs: 100,
    jsToolAuthority: { configuredMode: "full_auto" },
    // The turn stages exactly the native command tool whose dispatch the
    // held external lease blocks; full_auto so the coordination wait — not
    // an approval — is what parks it.
    profiles: [
      testProfile("lease-close", {
        tools: ["run_command"],
        permissionMode: "full_auto",
      }),
    ],
  });
  try {
    await service.registerCodingTools({
      workspaceRoot: workspace,
      sandbox: "off",
      configuredMode: "full_auto",
      coordinationDir: coordination,
      coordinationWaitMs: 10_000,
    });
    let requests = 0;
    service.setProvider(async () => {
      requests += 1;
      return requests === 1
        ? {
            content: "",
            finishReason: "tool_calls",
            toolCalls: [
              {
                id: "held-command",
                name: "run_command",
                arguments: { command: "echo effect > effect" },
              },
            ],
          }
        : { content: "done", finishReason: "stop", toolCalls: [] };
    });
    const session = await service.createSession({
      profileId: "lease-close",
      model: "test",
      executionMode: "full_auto",
    });
    const sent = await service.send(session.sessionId as string, {
      content: { type: "text", text: "run" },
    });
    if (sent.kind !== "turn") throw new Error("expected native command turn");
    await waitForAdmission(store, sent.turnId, locks);
    expect(existsSync(join(workspace, "effect"))).toBe(false);
    const started = Date.now();
    await service.close();
    expect(Date.now() - started).toBeLessThan(3_000);
    expect(service.closeUncertain).toBe(false);
    expect(readFileSync(lock, "utf8")).toBe(owner);
    const refusal = await receipt(store, sent.turnId);
    expect(refusal?.status).toBe("completed");
    expect(refusal?.result?.ok).toBe(false);
    expect(refusal?.result?.data?.errorCode).toBe("command_not_started");
    expect(refusal?.result?.data?.started).toBe(false);
    expect(
      readdirSync(locks).filter((name) => name.startsWith("intent-")),
    ).toEqual([]);
    await store.close();
    const reopened = await NativeStore.open(path);
    await reopened.close();
    expect(existsSync(join(workspace, "effect"))).toBe(false);
  } finally {
    rmSync(lock, { force: true });
    await service.close().catch(() => {});
    await store.close().catch(() => {});
    rmSync(directory, { recursive: true, force: true });
  }
}, 15_000);
