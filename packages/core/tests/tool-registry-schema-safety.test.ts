import { describe, expect, it } from "bun:test";
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

/** A schema `depth` levels deep, built from nested `properties`. */
function deepSchema(depth: number): AiJsonSchemaObject {
  let node: AiJsonSchemaObject = { type: "string" };
  for (let i = 0; i < depth; i++)
    node = { type: "object", properties: { next: node } };
  return node;
}

describe("AiToolRegistry schema hygiene", () => {
  it("compiles a schema declaring the 2020-12 dialect", () => {
    // What a spec-conformant MCP server sends. Ajv is draft-07 here and used to
    // throw on the unknown dialect, dropping every tool of that server.
    const r = new AiToolRegistry();
    const schema = {
      $schema: "https://json-schema.org/draft/2020-12/schema",
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    } as unknown as AiJsonSchemaObject;
    expect(() => r.register(makeTool("mcp_tool", schema))).not.toThrow();
    expect(r.validateInput("mcp_tool", {})).not.toEqual([]);
    expect(r.validateInput("mcp_tool", { text: "hi" })).toEqual([]);
  });

  it("registers two tools that declare the same $id", () => {
    // A shared Ajv keyed $id globally: the second registration threw and took
    // the whole tool-set build with it.
    // Two distinct documents claiming one $id — what two MCP servers built from
    // the same generator produce.
    const schema = (property: string): AiJsonSchemaObject =>
      ({
        $id: "https://example.test/args.json",
        type: "object",
        properties: { [property]: { type: "string" } },
      }) as unknown as AiJsonSchemaObject;
    const r = new AiToolRegistry();
    r.register(makeTool("first", schema("a")));
    expect(() => r.register(makeTool("second", schema("b")))).not.toThrow();
    expect(r.size()).toBe(2);
  });

  it("validates formats instead of ignoring them", () => {
    const r = new AiToolRegistry();
    r.register(
      makeTool("mailer", {
        type: "object",
        properties: { to: { type: "string", format: "email" } },
        required: ["to"],
      } as unknown as AiJsonSchemaObject),
    );
    expect(r.validateInput("mailer", { to: "someone@example.test" })).toEqual(
      [],
    );
    expect(r.validateInput("mailer", { to: "not-an-email" })).not.toEqual([]);
  });

  it("refuses a schema nested past the depth bound", () => {
    const r = new AiToolRegistry();
    expect(() => r.register(makeTool("deep", deepSchema(40)))).toThrow(
      /nesting exceeds/,
    );
    expect(r.size()).toBe(0);
  });

  it("refuses a schema with too many nodes", () => {
    const properties: Record<string, AiJsonSchemaObject> = {};
    for (let i = 0; i < 3000; i++) properties[`p${i}`] = { type: "string" };
    const r = new AiToolRegistry();
    expect(() =>
      r.register(makeTool("wide", { type: "object", properties })),
    ).toThrow(/exceeds 2000 nodes/);
  });

  it("accepts an ordinary schema well within the bounds", () => {
    const r = new AiToolRegistry();
    expect(() => r.register(makeTool("normal", deepSchema(10)))).not.toThrow();
  });
});
