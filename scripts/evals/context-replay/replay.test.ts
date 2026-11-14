import { describe, expect, test } from "bun:test";
import { bill, type CapturedSession, classify, replay } from "./replay";

describe("replay", () => {
  test("prices cache reads separately, preserves unknowns and bounds", () => {
    const result = bill(
      {
        input_uncached: 20,
        cache_read: 80,
        cache_write_short: 0,
        cache_write_long: 0,
        output: 10,
        reasoning: null,
      },
      {
        input_uncached: 2,
        cache_read: 0.2,
        cache_write_short: 3,
        cache_write_long: 4,
        output: 8,
        reasoning: 9,
      },
    );
    expect(result.lower).toBe(136);
    expect(result.upper).toBeNull();
    expect(result.unknown).toEqual(["reasoning"]);
  });
  test("bounds unknown input allocation without assuming zero", () => {
    const result = bill(
      {
        input_uncached: null,
        cache_read: 20,
        cache_write_short: 0,
        cache_write_long: 0,
        output: 0,
        reasoning: 0,
      },
      {
        input_uncached: 2,
        cache_read: 0.2,
        cache_write_short: 3,
        cache_write_long: 4,
        output: 8,
        reasoning: 9,
      },
      100,
    );
    expect(result.lower).toBe(164);
    expect(result.upper).toBe(164);
    expect(result.unknown).toEqual(["input_uncached"]);
  });
  test("classifies roles and leaves ambiguous metadata unattributed", () => {
    const classes = classify({
      tools: [{ name: "read" }],
      messages: [
        { role: "system", content: "rules" },
        { role: "user", content: "old" },
        { role: "assistant", content: "answer" },
        { role: "user", content: "new" },
      ],
      selected_content: [
        { role: "system", content: "project", metadata: { kind: "project" } },
        { role: "system", content: "unknown" },
      ],
    });
    for (const key of [
      "tools",
      "system",
      "project",
      "history",
      "turn_context",
      "unattributed",
    ] as const)
      expect(classes[key]).toBeGreaterThan(0);
    expect(classes.memory).toBeNull();
  });
  test("replay deterministic, cache hit, capture identity and outcome retained", () => {
    const session: CapturedSession = {
      version: 1,
      id: "s",
      kind: "chat",
      outcome: "completed",
      requests: [
        {
          at_ms: 0,
          cache_key: "s",
          request: { messages: [{ role: "user", content: "long".repeat(50) }] },
          response: { content: "ok" },
          follow_handles: null,
        },
        {
          at_ms: 1,
          cache_key: "s",
          request: { messages: [{ role: "user", content: "long".repeat(50) }] },
          response: { content: "ok" },
          follow_handles: null,
        },
      ],
    };
    const one = replay(session, { lifetime_ms: 100, minimum_tokens: 1 });
    expect(one).toEqual(
      replay(JSON.parse(JSON.stringify(session)), {
        lifetime_ms: 100,
        minimum_tokens: 1,
      }),
    );
    expect(one.requests[0]?.request).toEqual(session.requests[0]?.request);
    expect(one.requests[0]?.usage.cache_read).toBe(0);
    expect(one.requests[1]?.usage.cache_read).toBeGreaterThan(0);
    expect(one.outcome).toBe("completed");
    expect(one.follow_handles).toBeNull();
  });
});

test("session report includes frozen illustrative prices and per-request bills", async () => {
  const { report, ILLUSTRATIVE_PRICES } = await import("./report");
  const capture: CapturedSession = {
    version: 1,
    id: "report",
    kind: "code",
    outcome: "script completed",
    requests: [
      {
        at_ms: 0,
        cache_key: "k",
        request: { messages: [{ role: "user", content: "hello world" }] },
        response: { content: "complete" },
        follow_handles: null,
      },
    ],
  };
  const result = report([capture], { lifetime_ms: 300000, minimum_tokens: 1 });
  expect(result.prices).toEqual(ILLUSTRATIVE_PRICES);
  expect(result.sessions[0]?.requests[0]?.bill.lower).toBeGreaterThan(0);
  expect(result.sessions[0]?.bill.upper).toBeNull();
  expect(result.sessions[0]?.class_totals.turn_context).toBeGreaterThan(0);
  expect(result.sessions[0]?.outcome).toBe("script completed");
});

test("classifies Responses host assembly without dropping instructions or input", () => {
  const classes = classify({
    instructions: "system instructions",
    input: [
      { type: "message", role: "user", content: "turn" },
      { type: "function_call", name: "read_file", arguments: "{}" },
    ],
    tools: [{ type: "function", name: "read_file" }],
  });
  expect(classes.system).toBeGreaterThan(0);
  expect(classes.turn_context).toBeGreaterThan(0);
  expect(classes.history).toBeGreaterThan(0);
  expect(classes.tools).toBeGreaterThan(0);
});

test("cache simulation isolates keys and expires at lifetime boundary", () => {
  const request = { messages: [{ role: "user", content: "x".repeat(100) }] };
  const entry = (at_ms: number, cache_key: string) => ({
    at_ms,
    cache_key,
    request,
    response: { content: "ok" },
    follow_handles: null,
  });
  const session: CapturedSession = {
    version: 1,
    id: "boundary",
    kind: "chat",
    outcome: "script completed",
    requests: [entry(0, "a"), entry(1, "b"), entry(99, "a"), entry(199, "a")],
  };
  const result = replay(session, { lifetime_ms: 100, minimum_tokens: 1 });
  expect(result.requests.map((item) => item.usage.cache_read)).toEqual([
    0,
    0,
    Math.floor(new TextEncoder().encode(JSON.stringify(request)).length / 4),
    0,
  ]);
});

function framedSession(): CapturedSession {
  const args = '{ "b": 2, "a": 1 }';
  return {
    version: 1,
    id: "synthetic",
    kind: "chat",
    outcome: "script completed",
    requests: [
      {
        at_ms: 0,
        cache_key: "synthetic",
        request: { messages: [{ role: "user", content: "hello" }] },
        response: { content: args },
        response_frame: {
          choices: [
            {
              delta: {
                tool_calls: [{ index: 0, function: { arguments: args } }],
              },
            },
          ],
        },
        output_evidence: { kind: "tool", value: args },
        follow_handles: null,
      },
    ],
  };
}

test("rejects response text changed independently of frozen provider frame", () => {
  const session = framedSession();
  const request = session.requests[0];
  if (!request) throw new Error("missing request");
  request.response = { content: "tampered" };
  expect(() =>
    replay(session, { lifetime_ms: 100, minimum_tokens: 1 }),
  ).toThrow();
});

test("prices output and reasoning as disjoint counters", () => {
  const result = bill(
    {
      input_uncached: 0,
      cache_read: 0,
      cache_write_short: 0,
      cache_write_long: 0,
      output: 10,
      reasoning: 4,
    },
    {
      input_uncached: 2,
      cache_read: 0.2,
      cache_write_short: 3,
      cache_write_long: 4,
      output: 8,
      reasoning: 9,
    },
  );
  expect(result.lower).toBe(116);
  expect(result.upper).toBe(116);
});

test("rejects frame when durable output evidence differs", () => {
  const session = framedSession();
  const request = session.requests[0];
  if (!request) throw new Error("missing request");
  request.output_evidence = { kind: "tool", value: "tampered" };
  expect(() =>
    replay(session, { lifetime_ms: 100, minimum_tokens: 1 }),
  ).toThrow();
});

test("rejects an arbitrary verifier argument before writing a report", async () => {
  const { mkdtempSync, existsSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const dir = mkdtempSync(join(tmpdir(), "agentkit-score-"));
  const output = join(dir, "bill.json");
  const child = Bun.spawnSync([
    "bun",
    new URL("./score.ts", import.meta.url).pathname,
    "input.json",
    output,
    "/missing/verifier",
  ]);
  expect(child.exitCode).not.toBe(0);
  expect(existsSync(output)).toBe(false);
  rmSync(dir, { recursive: true });
});

test("rejects unsupported multi-tool frame instead of undercounting output", () => {
  const session = framedSession();
  const request = session.requests[0];
  if (!request) throw new Error("missing request");
  const frame = request.response_frame as {
    choices: { delta: { tool_calls: unknown[] } }[];
  };
  frame.choices[0]?.delta.tool_calls.push({
    index: 1,
    function: { arguments: '{"extra":true}' },
  });
  expect(() =>
    replay(session, { lifetime_ms: 100, minimum_tokens: 1 }),
  ).toThrow();
});

test("scores the captured corpus while preserving unknown reasoning", async () => {
  const { readFileSync } = await import("node:fs");
  const { report } = await import("./report");
  const path = new URL(
    "../../../docs/evidence/context-efficiency/0.4/sessions.json",
    import.meta.url,
  );
  const sessions = JSON.parse(readFileSync(path, "utf8")) as CapturedSession[];
  expect(sessions.filter((session) => session.kind === "chat")).toHaveLength(5);
  expect(sessions.filter((session) => session.kind === "code")).toHaveLength(5);
  expect(new Set(sessions.map((session) => session.id)).size).toBe(10);
  expect(sessions.every((session) => session.requests.length === 6)).toBe(true);
  const result = report(sessions, {
    lifetime_ms: 300_000,
    minimum_tokens: 256,
  });
  for (const session of result.sessions) {
    expect(session.totals.reasoning).toBeNull();
    expect(Number.isFinite(session.bill.lower)).toBe(true);
    expect(session.bill.lower).toBeGreaterThan(0);
    expect(session.bill.upper).toBeNull();
  }
});

test("rejects Responses output containing tool arguments and text", () => {
  const session = framedSession();
  const request = session.requests[0];
  if (!request) throw new Error("missing request");
  request.response_frame = {
    response: { output: [{ arguments: "{}", content: [{ text: "ignored" }] }] },
  };
  request.response = { content: "{}" };
  request.output_evidence = { kind: "tool", value: "{}" };
  expect(() =>
    replay(session, { lifetime_ms: 100, minimum_tokens: 1 }),
  ).toThrow();
});

test("frozen baseline equals deterministic replay with recorded configuration", async () => {
  const { readFileSync } = await import("node:fs");
  const { report } = await import("./report");
  const root = new URL(
    "../../../docs/evidence/context-efficiency/0.4/",
    import.meta.url,
  );
  const sessions = JSON.parse(
    readFileSync(new URL("sessions.json", root), "utf8"),
  ) as CapturedSession[];
  const baseline = JSON.parse(
    readFileSync(new URL("baseline.json", root), "utf8"),
  );
  expect(
    report(sessions, { lifetime_ms: 300_000, minimum_tokens: 256 }),
  ).toEqual(baseline);
});
