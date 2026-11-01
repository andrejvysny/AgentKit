/**
 * AGENTKIT-56: the MCP surface of the native binding, both directions.
 *
 * SERVER: the Rust-hosted streamable-HTTP MCP server on `SessionService`
 * is exercised with the official SDK CLIENT package
 * (`@modelcontextprotocol/client`) — initialize, gated `tools/list`,
 * `tools/call` through the service's own JS tool lane, bearer refusal,
 * per-client session reuse, and clean stop/close.
 *
 * CLIENT: `NativeStore.mcpConnect` spawns the crate's stdio connector at
 * a real remote server built on the SDK SERVER package (the fixture);
 * descriptors carry the HOST effect (never the remote's annotation),
 * `mcpCallTool` round-trips, invalid arguments are refused LOCALLY, the
 * descriptors ride the EXISTING `registerTool` lane so the durable
 * authority still gates write calls, and disconnect/reconnect and store
 * close clean up.
 */
import { expect, test } from "bun:test";
import { mkdtempSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import { NativeStore } from "../src/index.js";
import { registerMcpTools } from "../src/index.js";
import type {
  NativeAgentError,
  NativeSessionService,
  ProviderRequest,
  ProviderResult,
  ToolDefinitionJson,
  ToolResultJson,
} from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

/** 32 bytes: the exact bearer floor the binding enforces. */
const TOKEN = "12345678901234567890123456789012";
const TOKEN_B = "abcdefghijklmnopabcdefghijklmnop";
const FIXTURE = join(import.meta.dir, "fixtures", "mcp-stdio-fixture.ts");

function codeOf(error: unknown): string | undefined {
  return error instanceof Error && "code" in error
    ? (error as NativeAgentError).code
    : undefined;
}

/** The fixture's payload rides in the host tool-result contract's `data`
 * slot (shape fixed by the binding's `outcome`). */
function textOf(result: ToolResultJson): string {
  const data = result.data as { text?: string } | null | undefined;
  return data?.text ?? "";
}

async function expectCode(
  run: () => Promise<unknown>,
  code: string,
): Promise<void> {
  try {
    await run();
  } catch (error) {
    expect(codeOf(error)).toBe(code);
    return;
  }
  throw new Error(`expected a rejection with code ${code}`);
}

function disposableDir(): string {
  return mkdtempSync(join(tmpdir(), "agentkit-mcp-"));
}

function fixtureLog(): { path: string; calls: () => string[] } {
  const path = join(disposableDir(), "fixture-calls.log");
  return {
    path,
    calls: () =>
      existsSync(path)
        ? readFileSync(path, "utf8").split("\n").filter((line) => line !== "")
        : [],
  };
}

async function withService(
  options?: Parameters<NativeStore["createService"]>[0],
): Promise<{ store: NativeStore; service: NativeSessionService }> {
  const store = await NativeStore.inMemory();
  const service = await store.createService(options);
  return { store, service };
}

/** Script a provider: one scripted result per model request. */
function scripted(...responses: unknown[]) {
  const calls: ProviderRequest[] = [];
  const provider = async (
    request: ProviderRequest,
  ): Promise<ProviderResult> => {
    const index = calls.length;
    calls.push(request);
    const response = responses[index];
    if (response === undefined)
      return { content: "script exhausted", finishReason: "stop", toolCalls: [] };
    return response as ProviderResult;
  };
  return { calls, provider };
}

function toolCall(id: string, name: string, args: unknown) {
  return {
    content: "",
    finishReason: "tool_calls",
    toolCalls: [{ id, name, arguments: args }],
  };
}

function final(text: string) {
  return { content: text, finishReason: "stop", toolCalls: [] };
}

async function awaitRunSettled(
  service: NativeSessionService,
  runId: string,
): Promise<void> {
  const subscription = await service.subscribeTurn(runId, 0);
  const started = Date.now();
  let done = false;
  while (!done && Date.now() - started < 20_000)
    done = (await subscription.poll(64, 250)).done;
  subscription.cancel();
  if (!done) throw new Error(`run ${runId} never settled`);
}

/**
 * Poll-and-backoff on the native runtime's own observable signals. Fake
 * timers cannot drive the Rust worker or a spawned stdio child, so
 * bounded polling on a real condition (never a guessed sleep) is the only
 * observable-signal wait available for these integration tests — the same
 * convention as `authority.test.ts`.
 */
async function waitFor<T>(
  read: () => Promise<T>,
  accept: (value: T) => boolean,
  timeoutMs = 10_000,
): Promise<T> {
  const started = Date.now();
  let value = await read();
  while (!accept(value) && Date.now() - started < timeoutMs) {
    await Bun.sleep(20);
    value = await read();
  }
  return value;
}

function waitingRequestId(state: Record<string, unknown>): string | null {
  const waiting = state.waiting as Record<string, unknown> | undefined;
  const id = waiting?.inputId;
  return typeof id === "string" ? id : null;
}

/** `Client` + `StreamableHTTPClientTransport` with the bearer header
 * attached: the crate authenticates EVERY request, not just initialize. */
function newMcpClient(url: string, token: string): [Client, StreamableHTTPClientTransport] {
  const client = new Client(
    { name: "native-mcp-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const transport = new StreamableHTTPClientTransport(new URL(url), {
    requestInit: { headers: { authorization: `Bearer ${token}` } },
  });
  return [client, transport];
}

const ECHO_READ: ToolDefinitionJson = {
  name: "echo",
  effect: "read",
  capability: "test.echo",
  description: "Echo a value",
  inputSchema: {
    type: "object",
    properties: { value: { type: "string" } },
    required: ["value"],
  },
};

const MARK_WRITE: ToolDefinitionJson = {
  name: "mark",
  effect: "write",
  capability: "test.mark",
  description: "Mark a value",
  inputSchema: {
    type: "object",
    properties: { note: { type: "string" } },
    required: ["note"],
  },
};

// ---------------------------------------------------------------------------
// Server: Rust-hosted MCP server, exercised by the official SDK client.
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: initialize, write-gated catalogue, engine round-trip, bearer refusal, session reuse",
  async () => {
    const { store, service } = await withService();
    const remoteRunIds: string[] = [];
    let echoCalls = 0;
    try {
      await service.registerTool(ECHO_READ, async (input, request) => {
        remoteRunIds.push(request.runId);
        echoCalls += 1;
        const value =
          typeof input === "object" &&
          input !== null &&
          "value" in input &&
          typeof input.value === "string"
            ? input.value
            : "";
        return { data: { echoed: value }, summary: `echo:${value}` };
      });
      await service.registerTool(MARK_WRITE, async () => {
        throw new Error("write tools must never execute while writesEnabled is false");
      });

      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      expect(info.port).toBeGreaterThan(0);
      expect(info.url).toBe(`http://127.0.0.1:${info.port}`);

      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);
      expect(client.getServerVersion()?.name).toBe("agentkit-mcp-server");
      const sessionId = transport.sessionId;
      expect(typeof sessionId).toBe("string");
      expect((sessionId as string).length).toBeGreaterThan(0);

      const listed = await client.listTools();
      const names = listed.tools.map((tool) => tool.name);
      expect(names).toContain("echo");
      expect(names).not.toContain("mark"); // writes gated by default

      const called = await client.callTool({
        name: "echo",
        arguments: { value: "remote" },
      });
      expect(called.isError).toBeFalsy();
      const text = JSON.stringify(called.content);
      expect(text).toContain("echo:remote");
      // Shape fixed by the binding's own `to_call_result`: it always
      // attaches the host result contract as structuredContent.
      const structured = called.structuredContent as { ok?: boolean };
      expect(structured.ok).toBe(true);
      expect(echoCalls).toBe(1);
      // The remote call executed through the service's JS tool lane under
      // a synthetic mcp run identity (not a fabricated durable run).
      expect(remoteRunIds[0]?.startsWith("mcp:")).toBe(true);

      // Unknown tool AND the hidden write tool are refused at call time.
      await expect(
        client.callTool({ name: "mark", arguments: { note: "x" } }),
      ).rejects.toThrow();
      await expect(
        client.callTool({ name: "not-there", arguments: {} }),
      ).rejects.toThrow();

      // Session reuse: the same client keeps one session across requests.
      await client.listTools();
      const calledAgain = await client.callTool({
        name: "echo",
        arguments: { value: "again" },
      });
      expect(JSON.stringify(calledAgain.content)).toContain("echo:again");
      expect(transport.sessionId).toBe(sessionId);
      expect(echoCalls).toBe(2);

      // A second bearer client gets its own session on the same token.
      const [second, secondTransport] = newMcpClient(info.url, TOKEN);
      await second.connect(secondTransport);
      expect(secondTransport.sessionId).not.toBe(sessionId);

      await client.close();
      await second.close();

      // Wrong and missing bearer credentials never reach the handler.
      const [wrong, wrongTransport] = newMcpClient(info.url, "x".repeat(32));
      await expect(wrong.connect(wrongTransport)).rejects.toThrow();
      const anonymous = await fetch(`${info.url}/`, {
        method: "POST",
        body: "{}",
      });
      expect(anonymous.status).toBe(401);

      await service.stopMcpServer();
      // Stopped: the listener is gone, follow-up connections are refused.
      await expect(fetch(`${info.url}/`, { method: "POST", body: "{}" })).rejects.toThrow();
      // Idempotent stop.
      await service.stopMcpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

nativeTest(
  "MCP server: writesEnabled opens write tools; option validation; close with a live server",
  async () => {
    const { store, service } = await withService();
    const marks: string[] = [];
    try {
      await service.registerTool(ECHO_READ, async () => ({ data: "read" }));
      await service.registerTool(MARK_WRITE, async (input) => {
        const note =
          typeof input === "object" &&
          input !== null &&
          "note" in input &&
          typeof input.note === "string"
            ? input.note
            : "";
        marks.push(note);
        return { data: "marked" };
      });

      const open = await service.startMcpServer({
        bearerTokens: [TOKEN, TOKEN_B],
        writesEnabled: true,
        allowedHosts: ["127.0.0.1", "localhost"],
        maxConcurrentCalls: 2,
        maxBatchSize: 4,
        callDeadlineMs: 30_000,
        idleTtlMs: 60_000,
        maxBodyBytes: 65536,
        maxSessionsPerPrincipal: 4,
      });

      const [client, transport] = newMcpClient(open.url, TOKEN_B);
      await client.connect(transport);
      const listed = await client.listTools();
      expect(listed.tools.map((tool) => tool.name)).toContain("mark");
      const called = await client.callTool({
        name: "mark",
        arguments: { note: "opened" },
      });
      expect(called.isError).toBeFalsy();
      expect(marks).toEqual(["opened"]);
      await client.close();

      // Second token authenticates against its own principal.
      const [other, otherTransport] = newMcpClient(open.url, TOKEN);
      await other.connect(otherTransport);
      await other.listTools();
      await other.close();

      await service.stopMcpServer();

      // Every weak or malformed configuration refuses as invalid_input.
      await expectCode(
        () => service.startMcpServer({ bearerTokens: [] }),
        "invalid_input",
      );
      await expectCode(
        () => service.startMcpServer({ bearerTokens: ["short"] }),
        "invalid_input",
      );
      await expectCode(
        () => service.startMcpServer({ bearerTokens: [TOKEN], port: 70000 }),
        "invalid_input",
      );
      await expectCode(
        () =>
          service.startMcpServer({
            bearerTokens: [TOKEN],
            maxConcurrentCalls: 0,
          }),
        "invalid_input",
      );
      await expectCode(
        () =>
          service.startMcpServer({
            bearerTokens: [TOKEN],
            allowedHosts: ["  "],
          }),
        "invalid_input",
      );

      // Exactly one server per service.
      await service.startMcpServer({ bearerTokens: [TOKEN] });
      await expectCode(
        () => service.startMcpServer({ bearerTokens: [TOKEN] }),
        "invalid_input",
      );
      await service.stopMcpServer();
    } finally {
      // `close()` with a live server: the MCP listener and its sessions
      // shut down BEFORE the bridge begins Closing (close never hangs).
      await service.startMcpServer({ bearerTokens: [TOKEN] });
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// Client: crate-managed connections to a real SDK-server fixture.
// ---------------------------------------------------------------------------

nativeTest(
  "MCP client: connect, host effect authority, call, local refusal, registerTool lane, disconnect",
  async () => {
    const dir = disposableDir();
    const log = `${dir}/calls.log`;
    const { store, service } = await withService({
      // Full-auto authority so the write-effect descriptor can act in the
      // engine round-trip below; the ask-mode gate is its own test.
      jsToolAuthority: { configuredMode: "full_auto" },
    });
    try {
      const connection = await store.mcpConnect({
        alias: "fixture",
        transport: {
          kind: "stdio",
          command: process.execPath,
          args: [FIXTURE],
          env: { AGENTKIT_FIXTURE_LOG: log },
        },
      });
      expect(connection.alias).toBe("fixture");
      const echo = connection.tools.find((tool) => tool.remoteName === "echo");
      const note = connection.tools.find((tool) => tool.remoteName === "write_note");
      const handleOf = (remote: string) =>
        connection.tools.find((tool) => tool.remoteName === remote)!.handle;
      expect(echo).toBeDefined();
      expect(note).toBeDefined();
      // Identity mirrors the crate's contributed-tool shape...
      expect(echo?.name).toBe("mcp__fixture__echo");
      expect(echo?.capability).toBe("mcp.fixture.echo");
      expect(echo?.inputSchema).toMatchObject({ type: "object" });
      // ...and the EFFECT is host-owned: the fixture annotates `echo`
      // readOnlyHint:true, yet without the host's readOnly allowance it
      // contributes as a write.
      expect(echo?.effect).toBe("write");
      expect(note?.effect).toBe("write");

      // Direct host call round-trips through the crate manager. (Shape
      // fixed by the binding's `outcome`: the host tool-result contract.)
      const called = await store.mcpCallTool(
        "fixture",
        "echo",
        { text: "hi" },
        handleOf("echo"),
      );
      const data = called.data as { text?: string; toolName?: string };
      expect(called.ok).toBe(true);
      expect(data.text).toBe("echo:hi");
      expect(data.toolName).toBe("echo");
      expect(existsSync(log)).toBe(true);

      // Invalid arguments are refused LOCALLY — the remote never sees the
      // call (the witness log stays at the one entry above).
      await expectCode(
        () => store.mcpCallTool("fixture", "echo", {}, handleOf("echo")),
        "mcp_invalid_arguments",
      );
      await expectCode(
        () => store.mcpCallTool("fixture", "echo", { text: 7 }, handleOf("echo")),
        "mcp_invalid_arguments",
      );
      expect(readFileSync(log, "utf8").trim().split("\n")).toEqual(["echo"]);
      // Unknown alias / unknown tool are typed not-found refusals.
      await expectCode(
        () => store.mcpCallTool("absent", "echo", { text: "x" }),
        "not_found",
      );
      await expectCode(
        () => store.mcpCallTool("fixture", "nope", {}),
        "not_found",
      );

      // The descriptors ride the EXISTING registerTool lane: registering
      // through `registerMcpTools` puts the tool in front of the Rust
      // authority, and its execution invokes the Rust manager.
      const registered = await registerMcpTools(service, store, connection);
      expect(registered).toEqual(connection.tools);
      const { calls, provider } = scripted(
        toolCall("call-1", "mcp__fixture__echo", { text: "engine" }),
        final("done"),
      );
      service.setProvider(provider);
      const created = await service.createSession({
        profileId: "mcp-client",
        model: "test-model",
      });
      const sessionId = created.sessionId as string;
      const sent = await service.send(sessionId, {
        content: { type: "text", text: "echo via engine" },
      });
      if (sent.kind !== "turn") throw new Error("expected a durable turn submission");
      await awaitRunSettled(service, sent.turnId);
      // The engine delivered the tool result into the model transcript.
      expect(JSON.stringify(calls[1]?.messages ?? [])).toContain("echo:engine");
      const logLines = readFileSync(log, "utf8").trim().split("\n");
      expect(logLines.filter((line) => line === "echo").length).toBe(2);

      // Manager view + disconnect + idempotency + reconnect.
      const servers = await store.mcpListServers();
      expect(servers).toEqual([
        {
          alias: "fixture",
          kind: "stdio",
          enabled: true,
          readOnly: false,
          connected: true,
          registration: 1,
          handle: "fixture#1",
          tools: 2,
        },
      ]);
      await store.mcpDisconnect("fixture");
      await store.mcpDisconnect("fixture"); // idempotent
      expect(await store.mcpListServers()).toEqual([]);
      await expectCode(
        () => store.mcpCallTool("fixture", "echo", { text: "x" }, "fixture#9"),
        "not_found",
      );
      const again = await store.mcpConnect({
        alias: "fixture",
        transport: { kind: "stdio", command: process.execPath, args: [FIXTURE] },
        readOnly: true,
      });
      expect(again.tools.every((tool) => tool.effect === "read")).toBe(true);
      // Reconnecting a known alias is refused until disconnected.
      await expectCode(
        () =>
          store.mcpConnect({
            alias: "fixture",
            transport: { kind: "stdio", command: process.execPath, args: [FIXTURE] },
          }),
        "invalid_input",
      );
      // A bad declaration is refused before any spawn (invalid JSON
      // shape → invalid_input from the binding parser).
      await expectCode(
        () => store.mcpConnect({ alias: "bad" } as never),
        "invalid_input",
      );
      await store.mcpDisconnect("fixture");
    } finally {
      await service.close();
      await store.close(); // tears down connected MCP servers too
    }
  },
  40_000,
);

nativeTest(
  "MCP client: host effect reaches the engine guard — write descriptor waits for approval",
  async () => {
    const dir = fixtureLog();
    // DEFAULT authority (ask mode): a write-effect tool cannot act
    // without one human approval — the guard chain applies to MCP-
    // contributed descriptors exactly as to any JS tool.
    const { store, service } = await withService();
    try {
      const connection = await store.mcpConnect({
        alias: "notes",
        transport: {
          kind: "stdio",
          command: process.execPath,
          args: [FIXTURE],
          env: { AGENTKIT_FIXTURE_LOG: dir.path },
        },
      });
      await registerMcpTools(service, store, connection);
      const { provider } = scripted(
        toolCall("call-1", "mcp__notes__echo", { text: "guarded" }),
        final("done"),
      );
      service.setProvider(provider);
      const created = await service.createSession({
        profileId: "mcp-guard",
        model: "test-model",
      });
      const sessionId = created.sessionId as string;
      const sent = await service.send(sessionId, {
        content: { type: "text", text: "guarded echo" },
      });
      // The write descriptor parks on a per-call approval BEFORE the
      // remote is ever contacted.
      const waiting = await waitFor(
        () => service.getState(sessionId),
        (state) => waitingRequestId(state) !== null,
      );
      expect(JSON.stringify(waiting.waiting)).toContain("mcp__notes__echo");
      expect(dir.calls()).toEqual([]);
      const requestId = waitingRequestId(waiting);
      if (requestId === null)
        throw new Error("the write tool never parked on an approval");
      await service.approve(requestId, "ans-1", "ok");
      if (sent.kind !== "turn") throw new Error("expected a durable turn submission");
      await awaitRunSettled(service, sent.turnId);
      expect(dir.calls()).toEqual(["echo"]);
    } finally {
      await service.close();
      await store.close();
    }
  },
  40_000,
);

// ---------------------------------------------------------------------------
// Registration generations: a descriptor must never authorize another
// generation's callback. Everything here runs through the real binding
// (`@agentkit/native`), a real stdio MCP server fixture, and the real
// registerTool/engine lane; the witness is the fixture's call log.
// ---------------------------------------------------------------------------

nativeTest(
  "MCP client: a superseded registration never authorizes the replacement server's callback",
  async () => {
    const log = fixtureLog();
    const { store, service } = await withService({
      jsToolAuthority: { configuredMode: "full_auto" },
    });
    const spawn = (env: Record<string, string>) => ({
      kind: "stdio" as const,
      command: process.execPath,
      args: [FIXTURE],
      env: { AGENTKIT_FIXTURE_LOG: log.path, ...env },
    });
    try {
      // Generation 1 behind alias `dual`, registered through the authority lane.
      const first = await store.mcpConnect({
        alias: "dual",
        transport: spawn({ AGENTKIT_FIXTURE_TAG: "gen1" }),
      });
      const echoOne = first.tools.find((tool) => tool.remoteName === "echo")!;
      expect(echoOne.handle).toBe("dual#1");
      expect(echoOne.name).toBe("mcp__dual__echo");
      await registerMcpTools(service, store, first);

      const session = (await service.createSession({
        profileId: "mcp-generations",
        model: "test-model",
      })) as { sessionId: string };
      const { calls, provider } = scripted(
        toolCall("call-1", "mcp__dual__echo", { text: "engine" }),
        final("done"),
        toolCall("call-2", "mcp__dual__echo", { text: "stale" }),
        final("after"),
      );
      service.setProvider(provider);

      const runOne = await service.send(session.sessionId, {
        content: { type: "text", text: "first generation" },
      });
      if (runOne.kind !== "turn") throw new Error("expected a durable turn submission");
      await awaitRunSettled(service, runOne.turnId);
      expect(JSON.stringify(calls[1]?.messages ?? [])).toContain("echo:gen1:engine");
      expect(log.calls()).toEqual(["echo"]);

      // The host now replaces the server behind the SAME alias: same tool
      // name, same descriptor name and capability, different callback (it
      // answers with a different payload and demands a different argument).
      await store.mcpDisconnect("dual");
      const second = await store.mcpConnect({
        alias: "dual",
        transport: spawn({ AGENTKIT_FIXTURE_TAG: "gen2", AGENTKIT_FIXTURE_ECHO: "word" }),
      });
      const echoTwo = second.tools.find((tool) => tool.remoteName === "echo")!;
      expect(echoTwo.handle).toBe("dual#2");
      expect(echoTwo.name).toBe(echoOne.name);
      expect(echoTwo.capability).toBe(echoOne.capability);

      // The registered tool still carries the generation-1 handle, so the
      // engine's second call is refused and the replacement never runs.
      const runTwo = await service.send(session.sessionId, {
        content: { type: "text", text: "second generation" },
      });
      if (runTwo.kind !== "turn") throw new Error("expected a durable turn submission");
      await awaitRunSettled(service, runTwo.turnId);
      // The engine sees the binding's refusal: the tool result is the
      // handle-rejection failure (the binding's message names the superseded
      // registration), never the replacement server's answer.
      const transcript = JSON.stringify(calls[3]?.messages ?? []);
      expect(transcript).toContain("superseded registration");
      expect(transcript).not.toContain("echo:gen2");
      expect(
        log.calls(),
        "the replacement callback ran for a stale descriptor",
      ).toEqual(["echo"]);

      // The same refusals hold on the direct host lane.
      await expectCode(
        () => store.mcpCallTool("dual", "echo", { text: "stale" }, echoOne.handle),
        "mcp_authority_revoked",
      );
      await expectCode(
        () => store.mcpCallTool("dual", "echo", { text: "stale" }),
        "mcp_authority_revoked",
      );
      await expectCode(
        () => store.mcpCallTool("dual", "echo", { text: "stale" }, "dual#7"),
        "mcp_authority_revoked",
      );
      expect(log.calls()).toEqual(["echo"]);

      // And the CURRENT registration reaches ONLY the current callback — with
      // generation 1's argument shape refused locally, proving the descriptor
      // set really is the new generation's.
      await expectCode(
        () => store.mcpCallTool("dual", "echo", { text: "stale" }, echoTwo.handle),
        "mcp_invalid_arguments",
      );
      const via2 = await store.mcpCallTool("dual", "echo", { word: "two" }, echoTwo.handle);
      expect(textOf(via2)).toBe("echo:gen2:two");
      expect(log.calls()).toEqual(["echo", "echo"]);
      expect((await store.mcpListServers()).find((entry) => entry.alias === "dual")?.registration).toBe(2);
    } finally {
      await service.close();
      await store.close();
    }
  },
  60_000,
);

nativeTest(
  "MCP client: an alias whose replacement name shadows another alias routes by (alias, generation)",
  async () => {
    const logA = fixtureLog();
    const logB = fixtureLog();
    const { store } = await withService();
    try {
      const a = await store.mcpConnect({
        alias: "a",
        transport: {
          kind: "stdio",
          command: process.execPath,
          args: [FIXTURE],
          env: { AGENTKIT_FIXTURE_LOG: logA.path },
        },
      });
      // Alias `b` RENAMES both of its tools (`echo -> shadow`,
      // `write_note -> note`). Identity, authorization and execution must all
      // use the same (name, alias, generation) tuple, so neither alias can
      // reach the other's callback.
      const b = await store.mcpConnect({
        alias: "b",
        toolAliases: { echo: "shadow", write_note: "note" },
        transport: {
          kind: "stdio",
          command: process.execPath,
          args: [FIXTURE],
          env: { AGENTKIT_FIXTURE_LOG: logB.path },
        },
      });
      const aEcho = a.tools.find((tool) => tool.remoteName === "echo")!;
      const bShadowed = b.tools.find((tool) => tool.remoteName === "write_note")!;
      const bRenamedAway = b.tools.find((tool) => tool.remoteName === "echo")!;
      expect(aEcho.name).toBe("mcp__a__echo");
      // The fixture annotates `echo` readOnlyHint:true; effect is HOST-owned,
      // so without the host's readOnly allowance it stays "write".
      expect(aEcho.effect).toBe("write");
      expect(bShadowed.name).toBe("mcp__b__note"); // renamed on the way out...
      expect(bShadowed.effect).toBe("write"); // ...but the contract is write_note's
      expect(bShadowed.capability).toBe("mcp.b.note");
      expect(bRenamedAway.name).toBe("mcp__b__shadow");

      // An AMBIGUOUS rename — a replacement that is itself a renamed-away
      // name (`echo -> shadow` alongside `write_note -> echo`) — denotes two
      // different tools with one name. It is refused at connect, before a
      // descriptor could advertise one callback while the resolver reaches
      // the other; and the refusal takes nothing down with it (finding 3).
      await expectCode(
        () =>
          store.mcpConnect({
            alias: "c",
            toolAliases: { echo: "shadow", write_note: "echo" },
            transport: {
              kind: "stdio",
              command: process.execPath,
              args: [FIXTURE],
              env: { AGENTKIT_FIXTURE_LOG: logA.path },
            },
          }),
        "mcp_invalid_config",
      );
      expect((await store.mcpListServers()).map((entry) => entry.alias)).toEqual(["a", "b"]);
      expect(
        (
          await store.mcpCallTool("a", "echo", { text: "alive" }, aEcho.handle)
        ).ok,
      ).toBe(true);
      expect(logA.calls()).toEqual(["echo"]);
      // Canonical identities stay distinct across the aliases.
      const names = new Set([...a.tools, ...b.tools].map((tool) => tool.name));
      const caps = new Set([...a.tools, ...b.tools].map((tool) => tool.capability));
      expect(names.size).toBe(4);
      expect(caps.size).toBe(4);

      // Execution side: b's shadowing descriptor runs b's write_note.
      const viaB = await store.mcpCallTool(
        "b",
        bShadowed.remoteName,
        { note: "kept" },
        bShadowed.handle,
      );
      expect(textOf(viaB)).toBe("saved:kept");
      expect(logB.calls()).toEqual(["write_note"]);
      expect(logA.calls(), "b's call reached alias a").toEqual(["echo"]);

      // And a's plain echo still runs a's echo — the shadow never hijacked it.
      const viaA = await store.mcpCallTool(
        "a",
        aEcho.remoteName,
        { text: "kept" },
        aEcho.handle,
      );
      expect(textOf(viaA)).toBe("echo:kept");
      expect(logA.calls()).toEqual(["echo", "echo"]);
      expect(logB.calls()).toEqual(["write_note"]);

      // The renamed-away descriptor still dispatches to b's OWN echo —
      // alias `a`'s identically named tool never hijacks it.
      const viaShadow = await store.mcpCallTool(
        "b",
        "echo",
        { text: "x" },
        bRenamedAway.handle,
      );
      expect(textOf(viaShadow)).toBe("echo:x");
      expect(logB.calls()).toEqual(["write_note", "echo"]);

      // Authorization is per (alias, generation): a live handle from one
      // alias cannot authorize a call on the other, even for the tool whose
      // EFFECTIVE name collides between them.
      await expectCode(
        () => store.mcpCallTool("b", "echo", { text: "x" }, aEcho.handle),
        "mcp_authority_revoked",
      );
      await expectCode(
        () => store.mcpCallTool("a", "write_note", { note: "x" }, bShadowed.handle),
        "mcp_authority_revoked",
      );
      // ...and nothing reached either server while those were refused.
      expect(logA.calls()).toEqual(["echo", "echo"]);
      expect(logB.calls()).toEqual(["write_note", "echo"]);
    } finally {
      await store.close();
    }
  },
  60_000,
);

nativeTest(
  "MCP client: a failed configuration change leaves the already-connected servers usable",
  async () => {
    const log = fixtureLog();
    const { store } = await withService();
    try {
      const keep = await store.mcpConnect({
        alias: "keep",
        transport: {
          kind: "stdio",
          command: process.execPath,
          args: [FIXTURE],
          env: { AGENTKIT_FIXTURE_LOG: log.path },
        },
      });
      const keepHandle = keep.tools.find((tool) => tool.remoteName === "echo")!.handle;
      expect((await store.mcpCallTool("keep", "echo", { text: "one" }, keepHandle)).data).toEqual(
        expect.objectContaining({ text: "echo:one", toolName: "echo" }),
      );

      // A new alias the crate REJECTS at validation (zero request timeout)
      // changes no state at all.
      await expectCode(
        () =>
          store.mcpConnect({
            alias: "doomed",
            resilience: { requestTimeoutMs: 0 },
            transport: {
              kind: "stdio",
              command: process.execPath,
              args: [FIXTURE],
              env: { AGENTKIT_FIXTURE_LOG: log.path },
            },
          }),
        "mcp_invalid_config",
      );
      // A new alias whose transport cannot start at all is reported as a
      // connection failure — and again takes nothing down with it.
      await expectCode(
        () =>
          store.mcpConnect({
            alias: "missing",
            transport: { kind: "stdio", command: "/nonexistent/agentkit-mcp-fixture", args: [] },
          }),
        "mcp_connect_failed",
      );
      // A secret-referencing declaration fails closed, never half-applied.
      await expectCode(
        () =>
          store.mcpConnect({
            alias: "secret",
            secretRefs: { TOKEN: "env://AGENTKIT_ABSENT" },
            transport: {
              kind: "stdio",
              command: process.execPath,
              args: [FIXTURE],
            },
          }),
        "mcp_secret_missing",
      );

      // `keep` survived every failed replacement untouched: same status, same
      // registration generation, still dispatchable on its original handle.
      const servers = await store.mcpListServers();
      expect(servers.map((entry) => entry.alias)).toEqual(["keep"]);
      expect(servers[0]).toMatchObject({
        connected: true,
        registration: 1,
        handle: "keep#1",
        tools: 2,
      });
      const again = await store.mcpCallTool("keep", "echo", { text: "two" }, keepHandle);
      expect(again.ok).toBe(true);
      expect(textOf(again)).toBe("echo:two");
      expect(log.calls()).toEqual(["echo", "echo"]);
    } finally {
      await store.close();
    }
  },
  60_000,
);
