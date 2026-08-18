import { describe, expect, it } from "bun:test";
import { AiToolRegistry } from "../src/tools/registry.js";
import type { AiTool } from "../src/tools/types.js";
import type { AiJsonSchemaObject } from "../src/json-schema.js";

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

const schema: AiJsonSchemaObject = {
  type: "object",
  additionalProperties: false,
  required: ["x"],
  properties: {
    x: { type: "number" },
    label: { type: "string", maxLength: 3 },
  },
};

describe("AiToolRegistry compiled validators", () => {
  it("compiles a validator at registration and exposes it", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("place_thing", schema));
    expect(typeof r.getValidator("place_thing")).toBe("function");
  });

  it("returns undefined for an unknown tool", () => {
    const r = new AiToolRegistry();
    expect(r.getValidator("missing")).toBeUndefined();
  });

  it("the compiled validator accepts good input and rejects bad input", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("place_thing", schema));
    const validate = r.getValidator("place_thing");
    expect(validate).toBeDefined();
    if (!validate) return;
    expect(validate({ x: 1, label: "ok" })).toBe(true);
    // missing required x
    expect(validate({ label: "ok" })).toBe(false);
    // maxLength overflow
    expect(validate({ x: 1, label: "toolong" })).toBe(false);
    // additionalProperties:false
    expect(validate({ x: 1, extra: true })).toBe(false);
  });
});

describe("AiToolRegistry.validateInput", () => {
  it("reuses the cached validator (same instance as getValidator)", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("place_thing", schema));
    const validate = r.getValidator("place_thing");
    expect(validate).toBeDefined();
    let calls = 0;
    const spied = ((...args: unknown[]) => {
      calls += 1;
      return (validate as (a: unknown) => boolean)(args[0]);
    }) as unknown as typeof validate;
    // Swap the cached validator behind a spy to prove validateInput uses it.
    (r as unknown as { validators: Map<string, unknown> }).validators.set(
      "place_thing",
      spied,
    );
    r.validateInput("place_thing", { x: 1, label: "ok" });
    expect(calls).toBe(1);
  });

  it("returns [] for valid input and errors for invalid input", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("place_thing", schema));
    expect(r.validateInput("place_thing", { x: 1, label: "ok" })).toEqual([]);
    const errors = r.validateInput("place_thing", { label: "toolong" });
    expect(errors.length).toBeGreaterThan(0);
    // mapped paths have no leading slash
    expect(errors.every((e) => !e.path.startsWith("/"))).toBe(true);
  });

  it("returns [] for an unknown tool", () => {
    const r = new AiToolRegistry();
    expect(r.validateInput("missing", { x: 1 })).toEqual([]);
  });

  it("does not half-register when the schema is malformed (no validator-less tool)", () => {
    const r = new AiToolRegistry();
    const bad = makeTool("bad_tool", {
      // invalid keyword value → Ajv.compile throws
      type: "object",
      properties: { x: { type: "not-a-type" } },
    } as unknown as AiJsonSchemaObject);
    expect(() => r.register(bad)).toThrow();
    // Neither map was mutated: tool absent AND no validator left behind.
    expect(r.has("bad_tool")).toBe(false);
    expect(r.get("bad_tool")).toBeUndefined();
    expect(r.getValidator("bad_tool")).toBeUndefined();
    expect(r.size()).toBe(0);
  });
});
