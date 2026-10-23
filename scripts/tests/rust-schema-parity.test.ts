import { describe, expect, it } from "bun:test";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import Ajv from "ajv";
import type { AiJsonSchemaObject } from "../../packages/contracts/src/index.js";
import * as contracts from "../../packages/contracts/src/schemas.js";
import { AiToolRegistry } from "../../packages/core/src/tools/registry.js";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
interface Outcome {
  decoded: boolean;
  compiled: boolean;
  valid: boolean;
}
interface Vector {
  id: string;
  profile: "tool" | "contract";
  schema: string;
  instance: Json;
  inputJson?: string;
  schemaJson?: string;
  knownDivergence?: string;
  expected: Outcome;
  expectedTypeScript?: Outcome;
}
interface Corpus {
  version: number;
  schemas: Record<string, AiJsonSchemaObject>;
  vectors: Vector[];
  compatibilityExceptions: Array<{ id: string; status: string }>;
}
const root = resolve(import.meta.dir, "../..");
const fixture = resolve(root, "rust/fixtures/schema-validation.json");

function evaluate(corpus: Corpus, vector: Vector): Outcome {
  let instance: unknown = vector.instance;
  try {
    if (vector.inputJson !== undefined) instance = JSON.parse(vector.inputJson);
  } catch {
    return { decoded: false, compiled: false, valid: false };
  }
  try {
    if (vector.profile === "contract") {
      const schemas = contracts as Record<string, object>;
      const schema = JSON.parse(
        JSON.stringify(schemas[vector.schema]),
      ) as object;
      const validate = new Ajv({ strict: true, allErrors: true }).compile(
        schema,
      );
      return {
        decoded: true,
        compiled: true,
        valid: Boolean(validate(instance)),
      };
    }
    const registry = new AiToolRegistry();
    registry.register({
      definition: {
        name: "corpus",
        version: "1",
        effect: "read",
        capability: "corpus",
        description: "Shared schema validation corpus",
        inputSchema:
          vector.schemaJson === undefined
            ? corpus.schemas[vector.schema]!
            : (JSON.parse(vector.schemaJson) as AiJsonSchemaObject),
      },
      async execute() {
        throw new Error("Schema corpus never executes tools");
      },
    });
    return {
      decoded: true,
      compiled: true,
      valid: registry.validateInput("corpus", instance).length === 0,
    };
  } catch {
    return { decoded: true, compiled: false, valid: false };
  }
}

describe("shared Rust/TypeScript schema corpus", () => {
  it("freezes acceptance from the actual TypeScript registry and contract validator", async () => {
    const corpus = JSON.parse(await readFile(fixture, "utf8")) as Corpus;
    expect(corpus.version).toBe(1);
    expect(corpus.vectors.length).toBe(221);
    expect(new Set(corpus.vectors.map((vector) => vector.id)).size).toBe(
      corpus.vectors.length,
    );
    for (const vector of corpus.vectors)
      expect(evaluate(corpus, vector), vector.id).toEqual(
        vector.expectedTypeScript ?? vector.expected,
      );
    const bundled = resolve(
      root,
      "rust/crates/contracts/tests/fixtures/schema-validation.json",
    );
    expect(JSON.parse(await readFile(bundled, "utf8"))).toEqual(corpus);
  });

  it("records the approved native rejection of unpaired UTF-16 surrogates", async () => {
    const corpus = JSON.parse(await readFile(fixture, "utf8")) as Corpus;
    expect(corpus.compatibilityExceptions).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: "rust-rejects-lone-surrogate",
          status: "user-approved",
        }),
      ]),
    );
    const exceptions = corpus.vectors.filter(
      (vector) => vector.knownDivergence === "rust-rejects-lone-surrogate",
    );
    expect(exceptions).toHaveLength(2);
    for (const vector of exceptions) {
      expect(vector.knownDivergence).toBe("rust-rejects-lone-surrogate");
      expect(evaluate(corpus, vector)).toEqual({
        decoded: true,
        compiled: true,
        valid: true,
      });
    }
  });

  it("preserves raw numeric overflow validation and JSON stringification", async () => {
    const corpus = JSON.parse(await readFile(fixture, "utf8")) as Corpus;
    const vectors = corpus.vectors.filter((vector) =>
      vector.id.includes("number-overflow"),
    );
    expect(vectors).toHaveLength(4);
    for (const vector of vectors) {
      expect(evaluate(corpus, vector), vector.id).toEqual(
        vector.expectedTypeScript ?? vector.expected,
      );
      expect(JSON.stringify(JSON.parse(vector.inputJson!))).toBe(
        vector.profile === "tool"
          ? "null"
          : '{"id":"i","title":"t","content":"c","priority":null}',
      );
    }
    expect(JSON.parse("1e-400")).toBe(0);
  });
});
