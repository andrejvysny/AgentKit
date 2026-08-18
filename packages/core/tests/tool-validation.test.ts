import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import {
  mapValidatorErrors,
  parseToolArguments,
  validateAgainstSchema,
  validateToolInput,
} from "../src/tools/validation.js";
import type { AiJsonSchemaObject } from "@agentkit/contracts";

// Representative WireEndpoint shape: each endpoint is either a "REF.PIN" string
// or an explicit { ref, pin } object. Ajv `oneOf` is required — the hand-rolled
// validateAgainstSchema cannot express this.
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
};

describe("validateToolInput (Ajv)", () => {
  it("accepts a valid WireEndpoint pair → []", () => {
    expect(
      validateToolInput(wireSchema, { source: "U1.OUT", target: "R1.1" }),
    ).toEqual([]);
  });

  it("flags an empty-object endpoint that matches neither oneOf branch", () => {
    const errors = validateToolInput(wireSchema, { source: {} });
    expect(errors.length).toBeGreaterThan(0);
  });

  it("catches an enum violation", () => {
    const schema = {
      type: "object",
      properties: { mode: { type: "string", enum: ["read", "write"] } },
    };
    expect(
      validateToolInput(schema, { mode: "delete" }).length,
    ).toBeGreaterThan(0);
    expect(validateToolInput(schema, { mode: "read" })).toEqual([]);
  });

  it("catches a maxLength overflow", () => {
    const schema = {
      type: "object",
      properties: { ref: { type: "string", maxLength: 3 } },
    };
    expect(
      validateToolInput(schema, { ref: "toolong" }).length,
    ).toBeGreaterThan(0);
    expect(validateToolInput(schema, { ref: "ok" })).toEqual([]);
  });

  it("catches an additionalProperties violation and names the key", () => {
    const schema = {
      type: "object",
      additionalProperties: false,
      properties: { ref: { type: "string" } },
    };
    const errors = validateToolInput(schema, { ref: "x", extra: 1 });
    expect(errors.length).toBeGreaterThan(0);
    expect(errors.some((e) => e.message.includes("extra"))).toBe(true);
  });

  it("maps error paths without a leading slash", () => {
    const schema = {
      type: "object",
      properties: { ref: { type: "string" } },
    };
    const errors = validateToolInput(schema, { ref: 1 });
    expect(errors.length).toBeGreaterThan(0);
    expect(errors[0]?.path.startsWith("/")).toBe(false);
  });

  it("returns [] when the schema is not an object", () => {
    expect(validateToolInput(undefined, { a: 1 })).toEqual([]);
    expect(validateToolInput(null, { a: 1 })).toEqual([]);
  });
});

describe("validateAgainstSchema (hand-rolled, preserved)", () => {
  const schema: AiJsonSchemaObject = {
    type: "object",
    required: ["name"],
    properties: { name: { type: "string" }, count: { type: "number" } },
  };

  it("accepts a valid object → []", () => {
    expect(validateAgainstSchema({ name: "x", count: 1 }, schema)).toEqual([]);
  });

  it("flags a missing required property", () => {
    const errors = validateAgainstSchema({ count: 1 }, schema);
    expect(errors.length).toBeGreaterThan(0);
  });

  it("flags a wrong type", () => {
    const errors = validateAgainstSchema({ name: 1 }, schema);
    expect(errors.length).toBeGreaterThan(0);
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
