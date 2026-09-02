import Ajv, { type ValidateFunction } from "ajv";
import addFormats from "ajv-formats";
import { TOOL_NAME_PATTERN } from "@agentkit/contracts";
import type { AiTool } from "./tool.js";
import { mapValidatorErrors, type ValidationError } from "./validation.js";

/**
 * Bounds on an inputSchema, checked before it reaches Ajv. Compilation cost
 * grows with the document, and a schema is not always the host's own: an MCP
 * server contributes whatever it likes.
 */
const MAX_SCHEMA_NODES = 2000;
const MAX_SCHEMA_DEPTH = 32;

/**
 * The tools one run may call, with their inputSchemas compiled once.
 *
 * SCHEMA TRUST: an inputSchema is not necessarily the host's own — an MCP
 * server contributes whatever JSON Schema it likes, and the arguments validated
 * against it come from the model. Two consequences a host must know about:
 *
 * - `pattern` (and `patternProperties`) is a ReDoS surface: Ajv compiles it to
 *   a real RegExp, and a catastrophic-backtracking pattern with model-authored
 *   input blocks the event loop. This registry does NOT rewrite or analyse
 *   patterns; a host staging untrusted tools should pre-filter them (drop the
 *   keyword, or vet the source) before registering.
 * - size is bounded here: a schema over {@link MAX_SCHEMA_NODES} nodes or
 *   {@link MAX_SCHEMA_DEPTH} deep is refused rather than compiled.
 */
export class AiToolRegistry {
  private readonly tools = new Map<string, AiTool>();
  /** Per-tool inputSchema validators, compiled once at registration. */
  private readonly validators = new Map<string, ValidateFunction>();

  register(tool: AiTool): void {
    const name = tool.definition.name;
    if (!TOOL_NAME_PATTERN.test(name)) {
      throw new Error(
        `Invalid tool name "${name}": must match ${TOOL_NAME_PATTERN.source} (no dots or spaces).`,
      );
    }
    if (this.tools.has(name)) {
      throw new Error(`Duplicate tool registration: "${name}"`);
    }
    // Compile the inputSchema exactly once, here, so per-call validation reuses
    // the precompiled validator instead of recompiling on every tool call.
    // Compile into a local FIRST so a malformed schema throws before either map
    // is touched — never leave a tool registered without its validator.
    const validate = compileInputSchema(name, tool.definition.inputSchema);
    this.tools.set(name, tool);
    this.validators.set(name, validate);
  }

  has(name: string): boolean {
    return this.tools.has(name);
  }

  get(name: string): AiTool | undefined {
    return this.tools.get(name);
  }

  /** The validator compiled for a tool's inputSchema, or `undefined` if unknown. */
  getValidator(name: string): ValidateFunction | undefined {
    return this.validators.get(name);
  }

  /**
   * Validate parsed `args` for tool `name` against its inputSchema, reusing the
   * validator compiled once at {@link register}. Returns `[]` when valid (or when
   * the tool is unknown — callers handle unknown-tool dispatch separately).
   */
  validateInput(name: string, args: unknown): ValidationError[] {
    const validate = this.validators.get(name);
    if (!validate) return [];
    validate(args);
    return mapValidatorErrors(validate);
  }

  list(): AiTool[] {
    return Array.from(this.tools.values());
  }

  listDefinitions() {
    return this.list().map((t) => t.definition);
  }

  size(): number {
    return this.tools.size;
  }
}

/**
 * Compile one tool's inputSchema on its OWN Ajv instance.
 *
 * Per tool, not shared: `$id` is a GLOBAL key on an Ajv instance, so two tools
 * declaring the same one made the second `register()` throw and took the whole
 * tool-set build down with it. Isolation costs one instance per tool and
 * removes a cross-tool failure mode entirely.
 *
 * `strict: false` because tool inputSchemas are authored loosely (unknown
 * keywords tolerated); `allErrors: true` mirrors validation.ts so callers see
 * every problem; `ajv-formats` so `format: "email"`/`"uri"`/... actually
 * validate instead of being ignored with a console warning.
 */
function compileInputSchema(
  toolName: string,
  inputSchema: AiTool["definition"]["inputSchema"],
): ValidateFunction {
  assertSchemaWithinBounds(toolName, inputSchema);
  const ajv = new Ajv({ allErrors: true, strict: false });
  addFormats(ajv);
  return ajv.compile(stripSchemaIdentity(inputSchema));
}

/**
 * Drop top-level `$id` and `$schema`.
 *
 * `$id` only names the schema in a registry this tool doesn't share any more,
 * and `$schema` names a DIALECT: an MCP server correctly advertising
 * `2020-12` (its own spec's dialect) made this draft-07 Ajv throw at compile
 * time, and the tool was silently dropped. Neither keyword changes how an
 * instance validates here.
 */
function stripSchemaIdentity(schema: object): Record<string, unknown> {
  const stripped: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(schema)) {
    if (key === "$id" || key === "$schema") continue;
    stripped[key] = value;
  }
  return stripped;
}

/** Refuse a schema too large or too deep to compile cheaply. */
function assertSchemaWithinBounds(toolName: string, schema: object): void {
  let nodes = 0;
  const walk = (node: unknown, depth: number): void => {
    if (node === null || typeof node !== "object") return;
    if (depth > MAX_SCHEMA_DEPTH) {
      throw new Error(
        `Invalid inputSchema for tool "${toolName}": nesting exceeds ${MAX_SCHEMA_DEPTH} levels.`,
      );
    }
    nodes += 1;
    if (nodes > MAX_SCHEMA_NODES) {
      throw new Error(
        `Invalid inputSchema for tool "${toolName}": exceeds ${MAX_SCHEMA_NODES} nodes.`,
      );
    }
    for (const value of Object.values(node as Record<string, unknown>))
      walk(value, depth + 1);
  };
  walk(schema, 1);
}
