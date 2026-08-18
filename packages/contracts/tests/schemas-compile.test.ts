import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import type { TSchema } from "@sinclair/typebox";
import * as schemas from "../src/schemas.js";
import {
  AiContextBindingSchema,
  AiRunEventSchema,
  AiSourceRefSchema,
  AiToolEnvelopeSchema,
  CONTRACT_VERSION,
  ProposalDtoSchema,
  REST_API_VERSION,
  REST_ROUTES,
  RunDtoSchema,
  type AiContextBinding,
  type AiSourceRef,
} from "../src/index.js";

/**
 * TypeBox schemas carry symbol-keyed internals (`[Kind]`, `[Optional]`). Ajv walks
 * string keys only, so it ignores them — but normalizing through JSON here keeps the
 * assertion honest: what we compile is exactly what a consumer would receive over
 * the wire / from a `.json` dump.
 */
const asJson = (schema: TSchema): object =>
  JSON.parse(JSON.stringify(schema)) as object;

const makeAjv = () => new Ajv({ strict: false, allErrors: true });

const entries = Object.entries(schemas) as Array<[string, TSchema]>;

describe("contract schemas", () => {
  it("exports a schema barrel that is non-empty and all values are schemas", () => {
    expect(entries.length).toBeGreaterThan(20);
    for (const [name, schema] of entries) {
      expect(name.endsWith("Schema")).toBe(true);
      expect(typeof schema).toBe("object");
    }
  });

  it("compiles every exported schema with Ajv", () => {
    const ajv = makeAjv();
    for (const [name, schema] of entries) {
      expect(() => {
        ajv.compile(asJson(schema));
      }, `schema ${name} must compile`).not.toThrow();
    }
  });

  it("pins the wire contract version", () => {
    expect(CONTRACT_VERSION).toMatch(/^\d+\.\d+\.\d+$/);
  });
});

describe("AiRunEventSchema validation", () => {
  const validate = makeAjv().compile(asJson(AiRunEventSchema));

  /** The v2 transport base every event carries, whatever it says. */
  const base = {
    runId: "run_1",
    timestamp: "2026-01-01T00:00:00.000Z",
    contractVersion: CONTRACT_VERSION,
    eventId: "evt_1",
    seq: 0,
  };

  it("accepts a well-formed run.started event", () => {
    expect(
      validate({
        type: "run.started",
        ...base,
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(true);
  });

  it("accepts the optional attemptId", () => {
    expect(
      validate({
        type: "run.started",
        ...base,
        attemptId: "attempt_2",
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(true);
  });

  it("rejects an event missing a base field", () => {
    // Every base field is load-bearing: without eventId a replayed stream cannot
    // be deduplicated, so it is required, not optional.
    const { eventId: _dropped, ...withoutEventId } = base;
    expect(
      validate({
        type: "run.started",
        ...withoutEventId,
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(false);
    const { seq: _alsoDropped, ...withoutSeq } = base;
    expect(
      validate({
        type: "run.started",
        ...withoutSeq,
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(false);
  });

  it("rejects a base field of the wrong type", () => {
    expect(
      validate({
        type: "run.started",
        ...base,
        seq: "first",
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(false);
  });

  it("accepts a run.warning event with an unrecognised (forward-compat) code", () => {
    expect(
      validate({
        type: "run.warning",
        ...base,
        data: { code: "some_future_code", message: "hi" },
      }),
    ).toBe(true);
  });

  it("accepts a run.usage event and rejects an out-of-vocabulary source", () => {
    const usage = {
      type: "run.usage",
      ...base,
      data: {
        callId: "call_1",
        attempt: 1,
        step: 2,
        model: "gpt-4o-mini",
        promptTokens: 10,
        completionTokens: 5,
        totalTokens: 15,
        source: "stream",
        finalForCall: true,
      },
    };
    expect(validate(usage)).toBe(true);
    // Token fields are optional — servers report them partially or not at all.
    expect(
      validate({
        ...usage,
        data: {
          callId: "call_1",
          attempt: 1,
          step: 0,
          model: "m",
          source: "response",
          finalForCall: false,
        },
      }),
    ).toBe(true);
    expect(
      validate({ ...usage, data: { ...usage.data, source: "telepathy" } }),
    ).toBe(false);
  });

  it("rejects a garbage object", () => {
    expect(validate({ nope: true })).toBe(false);
  });

  it("rejects a known event type with the wrong data shape", () => {
    expect(
      validate({
        type: "run.started",
        ...base,
        data: { model: 42, toolCount: "two" },
      }),
    ).toBe(false);
  });

  it("rejects an unknown event type", () => {
    expect(validate({ type: "run.exploded", ...base, data: {} })).toBe(false);
  });
});

describe("host-defined kinds", () => {
  type EdaSourceKind = "schematic" | "footprint";
  type EdaBindingKind = "design" | "selection";

  it("validates any host `kind` string on the wire", () => {
    const ajv = makeAjv();
    const sourceRef = ajv.compile(asJson(AiSourceRefSchema));
    const binding = ajv.compile(asJson(AiContextBindingSchema));

    // A kind this contract has never heard of still validates — the vocabulary
    // belongs to the host, not the framework.
    expect(
      sourceRef({ id: "s1", kind: "invoice-line", label: "Line 7" }),
    ).toBe(true);
    expect(
      binding({
        id: "b1",
        kind: "spreadsheet",
        refId: "sheet-3",
        label: "Q3",
        role: "primary",
        status: "active",
      }),
    ).toBe(true);
    // `kind` is still required, and still a string.
    expect(sourceRef({ id: "s1", label: "no kind" })).toBe(false);
    expect(sourceRef({ id: "s1", kind: 7, label: "numeric kind" })).toBe(false);
    // `role`/`status` stay closed unions — those are framework concepts.
    expect(
      binding({
        id: "b1",
        kind: "spreadsheet",
        refId: "sheet-3",
        label: "Q3",
        role: "sidekick",
        status: "active",
      }),
    ).toBe(false);
  });

  it("narrows `kind` at the type level via the type parameter", () => {
    const ref: AiSourceRef<EdaSourceKind> = {
      id: "s1",
      kind: "footprint",
      label: "R_0402",
    };
    const binding: AiContextBinding<EdaBindingKind> = {
      id: "b1",
      kind: "design",
      refId: "d1",
      label: "Main board",
      role: "primary",
      status: "active",
    };
    // The unparameterised alias still accepts them (default K = string).
    const widened: AiSourceRef[] = [ref];
    expect(widened[0]?.kind).toBe("footprint");
    expect(binding.kind).toBe("design");
  });
});

describe("AiToolEnvelopeSchema validation", () => {
  const validate = makeAjv().compile(asJson(AiToolEnvelopeSchema));

  it("accepts a well-formed envelope", () => {
    expect(
      validate({
        ok: true,
        status: "ok",
        summary: "did the thing",
        warnings: [],
        truncated: false,
        data: { anything: [1, 2, 3] },
      }),
    ).toBe(true);
  });

  it("accepts an envelope without the optional summary", () => {
    expect(
      validate({
        ok: false,
        status: "error",
        warnings: ["nope"],
        truncated: false,
        data: null,
      }),
    ).toBe(true);
  });

  it("rejects an envelope with an out-of-vocabulary status", () => {
    expect(
      validate({
        ok: true,
        status: "kinda",
        warnings: [],
        truncated: false,
        data: {},
      }),
    ).toBe(false);
  });

  it("rejects an envelope missing required fields", () => {
    expect(validate({ ok: true })).toBe(false);
  });
});

describe("REST v1 surface", () => {
  it("exports every REST DTO schema through the schema barrel", () => {
    // The barrel test above compiles whatever is exported; this one is the
    // guard against a DTO being added to rest.ts and never re-exported, which
    // would leave it uncompiled and unvalidated forever.
    const exported = new Set(Object.keys(schemas));
    for (const name of [
      "RunStatusDtoSchema",
      "ProposalStatusDtoSchema",
      "RiskLevelDtoSchema",
      "ChatDtoSchema",
      "MessageDtoSchema",
      "MessagePageDtoSchema",
      "RunDtoSchema",
      "ProposalDecisionDtoSchema",
      "ApplyOutcomeDtoSchema",
      "ProposalDtoSchema",
      "ToolEventDtoSchema",
      "ToolDefinitionDtoSchema",
      "CreateChatRequestSchema",
      "SubmitMessageRequestSchema",
      "SubmitMessageResponseSchema",
      "ProposalDecisionRequestSchema",
      "ApplyProposalRequestSchema",
      "VersionDtoSchema",
      "ProblemDetailsDtoSchema",
      "RunEventFrameDtoSchema",
    ]) {
      expect(exported.has(name), `${name} must be in the schema barrel`).toBe(
        true,
      );
    }
  });

  it("declares a route table whose paths all sit under the API version", () => {
    expect(REST_API_VERSION).toBe("v1");
    const routes = Object.entries(REST_ROUTES);
    expect(routes.length).toBe(17);
    for (const [name, route] of routes) {
      expect(route.path.startsWith(`/${REST_API_VERSION}/`), name).toBe(true);
      expect(["GET", "POST", "PATCH", "DELETE"]).toContain(route.method);
    }
    // Every path+method pair is distinct: two operations on one route would be
    // a table that cannot be turned into a router.
    const pairs = routes.map(([, r]) => `${r.method} ${r.path}`);
    expect(new Set(pairs).size).toBe(pairs.length);
    // Reading a route yields literals, not `string` — the point of `satisfies`.
    const path: "/v1/runs/:runId" = REST_ROUTES.getRun.path;
    expect(path).toBe("/v1/runs/:runId");
  });

  describe("RunDtoSchema validation", () => {
    const validate = makeAjv().compile(asJson(RunDtoSchema));

    it("accepts a completed run with its optional timestamps", () => {
      expect(
        validate({
          runId: "run_1",
          chatId: "chat_1",
          scopeId: "chat_1",
          status: "completed",
          createdAt: "2026-01-01T00:00:00.000Z",
          startedAt: "2026-01-01T00:00:01.000Z",
          finishedAt: "2026-01-01T00:00:09.000Z",
        }),
      ).toBe(true);
      // A queued run has neither — both are optional for exactly this case.
      expect(
        validate({
          runId: "run_2",
          chatId: "chat_1",
          scopeId: "chat_1",
          status: "queued",
          createdAt: "2026-01-01T00:00:00.000Z",
        }),
      ).toBe(true);
    });

    it("rejects a status outside the six-state vocabulary", () => {
      expect(
        validate({
          runId: "run_1",
          chatId: "chat_1",
          scopeId: "chat_1",
          status: "thinking",
          createdAt: "2026-01-01T00:00:00.000Z",
        }),
      ).toBe(false);
    });
  });

  describe("ProposalDtoSchema validation", () => {
    const validate = makeAjv().compile(asJson(ProposalDtoSchema));

    const applied = {
      id: "prp_1",
      chatId: "chat_1",
      runId: "run_1",
      scopeKey: "doc-42",
      actionId: "append_note-1_doc-42",
      toolName: "notes_append",
      kind: "notes.append",
      risk: "medium",
      status: "applied",
      summary: 'Append "hello"',
      warnings: [],
      truncated: false,
      decision: {
        actor: "policy",
        policyId: "session-write-policy",
        decidedAt: "2026-01-01T00:00:02.000Z",
      },
      outcome: { status: "partial", appliedOps: 1, failedOps: [] },
      createdAt: "2026-01-01T00:00:01.000Z",
      decidedAt: "2026-01-01T00:00:02.000Z",
      appliedAt: "2026-01-01T00:00:03.000Z",
    };

    it("accepts a fully-decided, applied proposal", () => {
      expect(validate(applied)).toBe(true);
    });

    it("accepts a bare pending proposal — everything optional is absent", () => {
      expect(
        validate({
          id: "prp_2",
          chatId: "chat_1",
          scopeKey: "doc-42",
          toolName: "notes_append",
          kind: "notes.append",
          risk: "low",
          status: "pending",
          warnings: [],
          truncated: false,
          createdAt: "2026-01-01T00:00:01.000Z",
        }),
      ).toBe(true);
    });

    it("rejects an out-of-vocabulary risk and a malformed outcome", () => {
      expect(validate({ ...applied, risk: "spicy" })).toBe(false);
      expect(
        validate({
          ...applied,
          outcome: { status: "applied", appliedOps: 1, failedOps: ["boom"] },
        }),
      ).toBe(false);
    });
  });
});
