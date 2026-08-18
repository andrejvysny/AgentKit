import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import { mapValidatorErrors, parseToolArguments } from "../src/tools/validation.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiJsonSchemaObject } from "@agentkit/contracts";

function makeTool(name: string, inputSchema: AiJsonSchemaObject): AiTool {
  return {
    definition: {
      name,
      version: "1",
      effect: "read",
      capability: "test",
      description: "test",
      inputSchema,
    },
    async execute(ctx) {
      return {
        ok: true,
        data: null,
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

// Representative WireEndpoint shape: each endpoint is either a "REF.PIN" string
// or an explicit { ref, pin } object. Ajv `oneOf` is required to express this.
const wireEndpoint = {
  oneOf: [
    { type: "string", maxLength: 64 },
    {
      type: "object",
      additionalProperties: false,
      required: ["ref", "pin"],
      properties: {
        ref: { type: "string" },
        pin: { type: "string" },
      },
    },
  ],
};

const wireSchema = {
  type: "object",
  additionalProperties: false,
  required: ["source", "target"],
  properties: {
    source: wireEndpoint,
    target: wireEndpoint,
  },
} as unknown as AiJsonSchemaObject;

describe("AiToolRegistry.validateInput (Ajv, full JSON-Schema coverage)", () => {
  it("accepts a valid WireEndpoint pair → []", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("wire_connect", wireSchema));
    expect(
      r.validateInput("wire_connect", { source: "U1.OUT", target: "R1.1" }),
    ).toEqual([]);
  });

  it("flags an empty-object endpoint that matches neither oneOf branch", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("wire_connect", wireSchema));
    const errors = r.validateInput("wire_connect", { source: {} });
    expect(errors.length).toBeGreaterThan(0);
  });

  it("catches an enum violation", () => {
    const schema = {
      type: "object",
      properties: { mode: { type: "string", enum: ["read", "write"] } },
    } as unknown as AiJsonSchemaObject;
    const r = new AiToolRegistry();
    r.register(makeTool("set_mode", schema));
    expect(r.validateInput("set_mode", { mode: "delete" }).length).toBeGreaterThan(
      0,
    );
    expect(r.validateInput("set_mode", { mode: "read" })).toEqual([]);
  });

  it("catches a maxLength overflow", () => {
    const schema: AiJsonSchemaObject = {
      type: "object",
      properties: { ref: { type: "string", maxLength: 3 } },
    };
    const r = new AiToolRegistry();
    r.register(makeTool("set_ref", schema));
    expect(r.validateInput("set_ref", { ref: "toolong" }).length).toBeGreaterThan(
      0,
    );
    expect(r.validateInput("set_ref", { ref: "ok" })).toEqual([]);
  });

  it("catches an additionalProperties violation and names the key", () => {
    const schema: AiJsonSchemaObject = {
      type: "object",
      additionalProperties: false,
      properties: { ref: { type: "string" } },
    };
    const r = new AiToolRegistry();
    r.register(makeTool("strict_ref", schema));
    const errors = r.validateInput("strict_ref", { ref: "x", extra: 1 });
    expect(errors.length).toBeGreaterThan(0);
    expect(errors.some((e) => e.message.includes("extra"))).toBe(true);
  });

  it("maps error paths without a leading slash", () => {
    const schema: AiJsonSchemaObject = {
      type: "object",
      properties: { ref: { type: "string" } },
    };
    const r = new AiToolRegistry();
    r.register(makeTool("typed_ref", schema));
    const errors = r.validateInput("typed_ref", { ref: 1 });
    expect(errors.length).toBeGreaterThan(0);
    expect(errors[0]?.path.startsWith("/")).toBe(false);
  });

  it("returns [] for a non-object schema: registration rejects it, so nothing is ever validated", () => {
    const r = new AiToolRegistry();
    // Ajv.compile throws on a non-object schema — register() must reject it
    // rather than silently leaving a validator-less tool behind.
    expect(() =>
      r.register(makeTool("bad_tool", undefined as unknown as AiJsonSchemaObject)),
    ).toThrow();
    // The tool never made it into the registry, so validateInput's unknown-tool
    // fallback (`[]`) is what a caller observes — same end result as the old
    // standalone validateToolInput's "no usable schema → []" guard.
    expect(r.validateInput("bad_tool", { a: 1 })).toEqual([]);
  });
});

describe("mapValidatorErrors", () => {
  const ajv = new Ajv({ allErrors: true, strict: false });

  it("returns [] when the validator passed", () => {
    const validate = ajv.compile({
      type: "object",
      properties: { x: { type: "number" } },
    });
    expect(validate({ x: 1 })).toBe(true);
    expect(mapValidatorErrors(validate)).toEqual([]);
  });

  it("maps a failing validator's errors without a leading slash", () => {
    const validate = ajv.compile({
      type: "object",
      properties: { x: { type: "number" } },
    });
    expect(validate({ x: "nope" })).toBe(false);
    const errors = mapValidatorErrors(validate);
    expect(errors.length).toBeGreaterThan(0);
    expect(errors.every((e) => !e.path.startsWith("/"))).toBe(true);
  });
});

describe("parseToolArguments (preserved)", () => {
  it("parses a JSON string", () => {
    expect(parseToolArguments('{"a":1}')).toEqual({
      ok: true,
      value: { a: 1 },
    });
  });

  it("reports invalid JSON", () => {
    const result = parseToolArguments("{not json}");
    expect(result.ok).toBe(false);
  });
});
