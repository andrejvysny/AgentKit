import { describe, expect, it } from "bun:test";
import { AiToolRegistry, ToolSchemaError } from "../src/tools/registry.js";
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

  it("registers two tools whose NESTED subschemas share an $id", () => {
    // The strip used to be top-level only, so a document repeating one `$id`
    // inside itself failed the compile ("resolves to more than one schema") and
    // staging dropped the tool without a word.
    const schema = {
      type: "object",
      properties: {
        from: { $id: "https://example.test/node.json", type: "string" },
        to: { $id: "https://example.test/node.json", type: "string" },
      },
      required: ["from"],
    } as unknown as AiJsonSchemaObject;
    const r = new AiToolRegistry();
    expect(() => r.register(makeTool("nested_ids", schema))).not.toThrow();
    expect(r.validateInput("nested_ids", { from: "a" })).toEqual([]);
    expect(r.validateInput("nested_ids", { from: 1 })).not.toEqual([]);
  });

  it("refuses an over-large schema with a typed ToolSchemaError", () => {
    const r = new AiToolRegistry();
    let thrown: unknown;
    try {
      r.register(makeTool("too_deep", deepSchema(40)));
    } catch (err) {
      thrown = err;
    }
    expect(thrown).toBeInstanceOf(ToolSchemaError);
    expect((thrown as ToolSchemaError).code).toBe("schema_too_large");
    expect((thrown as ToolSchemaError).toolName).toBe("too_deep");
  });

  it("refuses an uncompilable schema with a typed ToolSchemaError", () => {
    const r = new AiToolRegistry();
    let thrown: unknown;
    try {
      r.register(
        makeTool("remote_ref", {
          type: "object",
          properties: { v: { $ref: "https://example.test/other.json" } },
        } as unknown as AiJsonSchemaObject),
      );
    } catch (err) {
      thrown = err;
    }
    expect(thrown).toBeInstanceOf(ToolSchemaError);
    expect((thrown as ToolSchemaError).code).toBe("schema_invalid");
  });

  it("compiles one schema once and shares the validator across registries", () => {
    // Staging builds a fresh registry per turn (per CALL, over MCP), so the
    // same handful of schemas were recompiled — Ajv instance, formats and all —
    // for every one of them.
    const schema = (): AiJsonSchemaObject =>
      ({
        type: "object",
        properties: { to: { type: "string", format: "email" } },
        required: ["to"],
      }) as unknown as AiJsonSchemaObject;
    const first = new AiToolRegistry();
    first.register(makeTool("mailer_one", schema()));
    const second = new AiToolRegistry();
    second.register(makeTool("mailer_two", schema()));

    expect(second.getValidator("mailer_two")).toBe(
      first.getValidator("mailer_one"),
    );
    // Shared, and still correct for both.
    expect(first.validateInput("mailer_one", { to: "a@b.test" })).toEqual([]);
    expect(second.validateInput("mailer_two", { to: "nope" })).not.toEqual([]);
  });

  it("compiles an unknown format without printing anything", () => {
    // `ajv-formats` warns once per property per compile for a format it does
    // not know — which, on a per-turn (or per-call) staging, is a console flood
    // rather than a diagnostic.
    const printed: unknown[] = [];
    const spies = ["log", "warn", "error", "info"] as const;
    const original = spies.map((key) => [key, console[key]] as const);
    for (const key of spies) {
      console[key] = (...args: unknown[]) => {
        printed.push(args);
      };
    }
    const r = new AiToolRegistry();
    try {
      r.register(
        makeTool("odd_format", {
          type: "object",
          properties: {
            v: { type: "string", format: "agentkit-not-a-real-format" },
          },
        } as unknown as AiJsonSchemaObject),
      );
    } finally {
      for (const [key, fn] of original) console[key] = fn;
    }

    expect(printed).toEqual([]);
    // Unknown formats stay permissive — only the noise went away.
    expect(r.validateInput("odd_format", { v: "anything" })).toEqual([]);
  });
});
