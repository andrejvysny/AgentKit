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

  it("accepts a well-formed run.started event", () => {
    expect(
      validate({
        type: "run.started",
        runId: "run_1",
        timestamp: "2026-01-01T00:00:00.000Z",
        data: { model: "gpt-4o-mini", toolCount: 2 },
      }),
    ).toBe(true);
  });

  it("accepts a run.warning event with an unrecognised (forward-compat) code", () => {
    expect(
      validate({
        type: "run.warning",
        runId: "run_1",
        timestamp: "2026-01-01T00:00:00.000Z",
        data: { code: "some_future_code", message: "hi" },
      }),
    ).toBe(true);
  });

  it("accepts a run.usage event and rejects an out-of-vocabulary source", () => {
    const usage = {
      type: "run.usage",
      runId: "run_1",
      timestamp: "2026-01-01T00:00:00.000Z",
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
        runId: "run_1",
        timestamp: "2026-01-01T00:00:00.000Z",
        data: { model: 42, toolCount: "two" },
      }),
    ).toBe(false);
  });

  it("rejects an unknown event type", () => {
    expect(
      validate({
        type: "run.exploded",
        runId: "run_1",
        timestamp: "2026-01-01T00:00:00.000Z",
        data: {},
      }),
    ).toBe(false);
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
