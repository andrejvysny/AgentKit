import { afterEach, describe, expect, test } from "bun:test";
import { statSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  CassetteError,
  createCassette,
  parseFixtureJson,
  runWithCassette,
  scrubDeep,
} from "../src/index.js";
import {
  ScriptedClient,
  ThrowingClient,
  collect,
  ev,
  makeTool,
  okResult,
  textTurn,
} from "./support/harness.js";

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-sec-"));
  tempDirs.push(dir);
  return join(dir, "fixture.json");
}

function request(
  model = "m",
  content = "go",
  extra: Record<string, unknown> = {},
) {
  return {
    runId: "run1",
    model,
    messages: [{ role: "user" as const, content }],
    ...extra,
  };
}

const CTX = { toolCallId: "t1" } as never;

interface RecordedProviderRequestView {
  messageShape: { role?: string }[];
  continuation: { present: boolean };
}

afterEach(async () => {
  for (const dir of tempDirs.splice(0))
    await rm(dir, { recursive: true, force: true });
});

describe("security: replay is offline by construction", () => {
  test("replay never touches the wrapped live client", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();

    const thrower = new ThrowingClient();
    const replay = createCassette({ mode: "replay", fixture: path });
    const events = await collect(
      replay.provider(thrower).streamChat(request() as never),
    );
    expect(events.length).toBeGreaterThan(0);
    expect(thrower.touched).toBe(false);
    await replay.close();
  });

  test("replay never executes tool bodies", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    await record
      .tool(makeTool("write_file", async () => okResult("wrote")))
      .execute(CTX, { path: "a", content: "x" });
    await record.close();

    let executed = false;
    const replay = createCassette({ mode: "replay", fixture: path });
    await replay
      .tool(
        makeTool("write_file", async () => {
          executed = true;
          return okResult("wrote");
        }),
      )
      .execute(CTX, { path: "a", content: "x" });
    expect(executed).toBe(false);
    await replay.close();
  });

  test("a cassette without a fixture cannot serve replay", () => {
    expect(() => createCassette({ mode: "replay" })).toThrow(CassetteError);
    expect(() => createCassette({ mode: "record" })).toThrow(CassetteError);
  });
});

describe("security: fixtures carry no secrets by default", () => {
  test("prompt and streamed text are absent under metadata capture", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([
      { events: textTurn("super-secret-model-text") },
    ]);
    await collect(
      record
        .provider(live)
        .streamChat(request("m", "my confidential prompt") as never),
    );
    await record.close();

    const text = await readFile(path, "utf8");
    expect(text).not.toContain("my confidential prompt");
    expect(text).not.toContain("super-secret-model-text");
    // but structure survives: roles and counts are recorded
    const fixture = parseFixtureJson(text, path);
    const first = fixture.effects[0] as {
      request?: RecordedProviderRequestView;
    };
    expect(first.request?.messageShape[0]?.role).toBe("user");
  });

  test("reasoning deltas are not recorded under metadata capture", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([
      {
        events: [
          ev("run.message.reasoning-delta", {
            delta: "hidden chain of thought",
          }),
          ...textTurn("answer"),
        ],
      },
    ]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).not.toContain("hidden chain of thought");
    // The event type name may remain in the trace; its content never does.
    const fixture = parseFixtureJson(text, path);
    const events = (
      fixture.effects[0] as { outcome: { events: { type: string }[] } }
    ).outcome.events;
    expect(events.some((e) => e.type === "run.message.reasoning-delta")).toBe(
      false,
    );
  });

  test("continuation state is never recorded — only its presence", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(
      record.provider(live).streamChat(
        request("m", "go", {
          continuation: {
            messageCount: 2,
            messageDigest: "a".repeat(64),
            inputItems: [{ opaque: "ENCRYPTED-PROVIDER-STATE" }],
          },
          continuationScope: { chatId: "chat-1", branchId: "branch-1" },
        }) as never,
      ),
    );
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).not.toContain("ENCRYPTED-PROVIDER-STATE");
    expect(text).not.toContain("inputItems");
    const fixture = parseFixtureJson(text, path);
    const first = fixture.effects[0] as {
      request?: RecordedProviderRequestView;
    };
    expect(first.request?.continuation.present).toBe(true);
  });

  test("credential-shaped keys are scrubbed from tool payloads even in content mode", async () => {
    const path = await fixturePath();
    const record = createCassette({
      mode: "record",
      fixture: path,
      capture: "content",
    });
    const leaky = makeTool("env_dump", async () =>
      okResult({
        PATH: "/usr/bin",
        api_key: "sk-live-leak",
        nested: { Authorization: "Bearer abc", keep: "visible" },
      }),
    );
    await record.tool(leaky).execute(CTX, {});
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).not.toContain("sk-live-leak");
    expect(text).not.toContain("Bearer abc");
    expect(text).toContain("visible");
  });

  test("content capture is explicit and persists text", async () => {
    const path = await fixturePath();
    const record = createCassette({
      mode: "record",
      fixture: path,
      capture: "content",
    });
    const live = new ScriptedClient([
      { events: textTurn("verbatim model text") },
    ]);
    await collect(
      record.provider(live).streamChat(request("m", "the prompt") as never),
    );
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).toContain("the prompt");
    expect(text).toContain("verbatim model text");
  });

  test("scrubDeep drops forbidden keys at any depth", () => {
    const scrubbed = scrubDeep({
      a: 1,
      b: { token: "t", ok: "k" },
      c: [{ state: "opaque", d: 2 }],
    }) as Record<string, unknown>;
    expect(scrubbed.a).toBe(1);
    expect("token" in (scrubbed.b as object)).toBe(false);
    expect("state" in ((scrubbed.c as unknown[])[0] as object)).toBe(false);
  });
});

describe("fixtures are validated, not trusted", () => {
  test("malformed fixture JSON fails with cassette_schema", async () => {
    const path = await fixturePath();
    await writeFile(path, "{ not json", "utf8");
    const replay = createCassette({ mode: "replay", fixture: path });
    const error = await collect(
      replay.provider(new ThrowingClient()).streamChat(request() as never),
    ).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_schema");
  });

  test("schema-violating fixture fails with cassette_schema", async () => {
    const path = await fixturePath();
    await writeFile(
      path,
      JSON.stringify({ schemaVersion: 2, capture: "metadata", effects: [] }),
      "utf8",
    );
    const replay = createCassette({ mode: "replay", fixture: path });
    const error = await collect(
      replay.provider(new ThrowingClient()).streamChat(request() as never),
    ).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_schema");
  });

  test("duplicate effect ids are rejected", async () => {
    const path = await fixturePath();
    const effect = {
      effectId: "dup",
      sequence: 0,
      structuralFingerprint: "x",
      fullFingerprint: "y",
      trace: { steps: [] },
      kind: "models",
      models: [],
    };
    await writeFile(
      path,
      JSON.stringify({
        schemaVersion: 1,
        capture: "metadata",
        effects: [effect, { ...effect, sequence: 1 }],
      }),
      "utf8",
    );
    const replay = createCassette({ mode: "replay", fixture: path });
    const error = await replay
      .provider(new ThrowingClient())
      .listModels()
      .catch((c) => c);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_schema");
  });
});

describe("runWithCassette finalization", () => {
  test("persists the fixture even when execute throws", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("partial") }]);
    const error = await runWithCassette({
      cassette: record,
      execute: async () => {
        await collect(record.provider(live).streamChat(request() as never));
        throw new Error("test failure");
      },
    }).catch((cause) => cause);
    expect((error as Error).message).toBe("test failure");
    // Effects recorded before the failure are persisted for debugging.
    const fixture = parseFixtureJson(await readFile(path, "utf8"), path);
    expect(fixture.effects.length).toBe(1);
  });

  test("strict replay fails on unconsumed effects through the helper too", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();

    const replay = createCassette({ mode: "replay", fixture: path });
    const error = await runWithCassette({
      cassette: replay,
      execute: async () => undefined,
    }).catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_extra");
  });
});

describe("security: recording hardening (C2)", () => {
  test("credential SHAPES inside free-form text are scrubbed under content capture", async () => {
    const path = await fixturePath();
    const record = createCassette({
      mode: "record",
      fixture: path,
      capture: "content",
    });
    const live = new ScriptedClient([
      {
        events: textTurn("contact me at ghp_abcdefghijklmnopqrstuvwxyz123456"),
      },
    ]);
    await collect(
      record
        .provider(live)
        .streamChat(request("m", "api_key=supersecretvalue12345678") as never),
    );
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).not.toContain("ghp_abcdefghijklmnopqrstuvwxyz123456");
    expect(text).not.toContain("supersecretvalue12345678");
    expect(text).toContain("[redacted-secret]");
  });

  test("redactPolicy replaces application-known sensitive values", async () => {
    const path = await fixturePath();
    const record = createCassette({
      mode: "record",
      fixture: path,
      capture: "content",
      redactPolicy: (key, _value) =>
        key === "customerEmail" ? "<redacted>" : undefined,
    });
    const tool = makeTool(
      "send",
      async () =>
        okResult({ customerEmail: "user@example.com", body: "hello" }) as never,
    );
    const wrapped = record.instrumentRegistry({
      list: () => [tool],
      listDefinitions: () => [tool.definition],
      get: (name: string) => (name === tool.definition.name ? tool : undefined),
      register: () => undefined,
      has: (name: string) => name === tool.definition.name,
    } as never);
    await wrapped
      .get("send")!
      .execute(CTX, { customerEmail: "user@example.com" });
    await collect(
      record
        .provider(new ScriptedClient([{ events: textTurn("ok") }]))
        .streamChat(request() as never),
    );
    await record.close();
    const text = await readFile(path, "utf8");
    expect(text).not.toContain("user@example.com");
    expect(text).toContain("<redacted>");
  });

  test("recording cap fails the run instead of growing unbounded", async () => {
    const path = await fixturePath();
    const record = createCassette({
      mode: "record",
      fixture: path,
      maxEffects: 1,
    });
    const live = new ScriptedClient([
      { events: textTurn("a") },
      { events: textTurn("b") },
    ]);
    const client = record.provider(live);
    await collect(client.streamChat(request() as never));
    const error = await collect(client.streamChat(request() as never)).catch(
      (cause) => cause,
    );
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_limit");
  });

  test("recordings never overwrite an existing fixture", async () => {
    const path = await fixturePath();
    await writeFile(path, "precious", "utf8");
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(record.provider(live).streamChat(request() as never));
    const error = await record.close().catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).message).toContain("already exists");
    expect(await readFile(path, "utf8")).toBe("precious");
  });

  test("fixtures are written owner-only", async () => {
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const live = new ScriptedClient([{ events: textTurn("hi") }]);
    await collect(record.provider(live).streamChat(request() as never));
    await record.close();
    // eslint-disable-next-line no-bitwise
    expect(statSync(path).mode & 0o777).toBe(0o600);
  });

  test("error outcomes are part of the recorded trace and pinned by verify", async () => {
    const path = await fixturePath();
    const boom = new Error("provider exploded");
    const record = createCassette({ mode: "record", fixture: path });
    const recorder = record.provider(
      new ScriptedClient([{ events: textTurn("partial"), error: boom }]),
    );
    await collect(recorder.streamChat(request() as never)).catch(
      () => undefined,
    );
    await record.close();

    // Identical live behavior passes verify (terminal error step matches).
    const okVerify = createCassette({ mode: "verify", fixture: path });
    await collect(
      okVerify
        .provider(
          new ScriptedClient([{ events: textTurn("partial"), error: boom }]),
        )
        .streamChat(request() as never),
    ).catch(() => undefined);
    await okVerify.close();

    // A run that no longer fails at that point diverges from the fixture.
    const badVerify = createCassette({ mode: "verify", fixture: path });
    const error = await collect(
      badVerify
        .provider(new ScriptedClient([{ events: textTurn("partial") }]))
        .streamChat(request() as never),
    )
      .then(() => badVerify.close())
      .catch((cause) => cause);
    expect(error).toBeInstanceOf(CassetteError);
    expect((error as CassetteError).code).toBe("cassette_mismatch");
  });
});
