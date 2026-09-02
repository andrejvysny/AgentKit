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

/** How many compiled validators the process-wide cache keeps — see {@link compileInputSchema}. */
const MAX_CACHED_VALIDATORS = 512;

/** Why an inputSchema was refused. */
export type ToolSchemaErrorCode = "schema_too_large" | "schema_invalid";

/**
 * An inputSchema this registry refuses to compile.
 *
 * Typed rather than a bare `Error` because the callers that matter are not
 * humans reading a message: `stageRegistry` (and the MCP bridge behind it)
 * turns a whole tool set into a run's registry, and a plain throw from one
 * malformed schema used to be indistinguishable from a bug — so the tool was
 * dropped silently. A code lets a caller report WHICH tool it lost and why.
 */
export class ToolSchemaError extends Error {
  readonly code: ToolSchemaErrorCode;
  /** The tool whose `inputSchema` was refused. */
  readonly toolName: string;

  constructor(
    code: ToolSchemaErrorCode,
    toolName: string,
    message: string,
    options?: { cause?: unknown },
  ) {
    super(message, options);
    this.name = "ToolSchemaError";
    this.code = code;
    this.toolName = toolName;
  }
}

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
 *   {@link MAX_SCHEMA_DEPTH} deep is refused with a {@link ToolSchemaError}
 *   rather than compiled.
 *
 * CHANGED IN 0.5.0: `format` keywords are now VALIDATED (`ajv-formats`), where
 * before they were ignored. A tool whose schema says `format: "email"` and whose
 * caller passes `"not-an-email"` now fails validation instead of executing.
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
 * Compiled validators, keyed by the schema they were compiled from and shared
 * by every registry in the process.
 *
 * Staging builds a fresh {@link AiToolRegistry} per turn — and the MCP bridge
 * stages once per `tools/call` — so the SAME handful of schemas were compiled
 * over and over, each compile costing its own Ajv instance plus `ajv-formats`.
 * Sharing the result is safe because a `ValidateFunction` is stateless between
 * calls: the only thing it writes is `validate.errors`, read synchronously by
 * {@link AiToolRegistry.validateInput} in the same tick.
 *
 * Bounded and LRU (a `Map` iterates in insertion order; a hit re-inserts), so a
 * host staging thousands of distinct schemas cannot grow it without limit.
 */
const validatorCache = new Map<string, ValidateFunction>();

/**
 * Compile one tool's inputSchema on its OWN Ajv instance, or reuse the
 * validator an identical schema already produced.
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
 *
 * BEHAVIOUR NOTE (0.5.0): with `ajv-formats` installed, a `format` keyword that
 * used to be ignored now REJECTS a non-conforming value. `logger: false` keeps
 * the other half of that change quiet — an UNKNOWN format logs a warning per
 * property per compile, which on a per-turn staging is a console flood, not a
 * diagnostic.
 */
function compileInputSchema(
  toolName: string,
  inputSchema: AiTool["definition"]["inputSchema"],
): ValidateFunction {
  const prepared = prepareSchema(toolName, inputSchema);
  // The prepared document itself is the key: a digest would be shorter but a
  // collision would silently validate one tool's arguments against another
  // tool's schema, and the node cap already bounds how big this gets.
  const key = JSON.stringify(prepared);
  const cached = validatorCache.get(key);
  if (cached !== undefined) {
    validatorCache.delete(key);
    validatorCache.set(key, cached);
    return cached;
  }
  const ajv = new Ajv({ allErrors: true, strict: false, logger: false });
  addFormats(ajv);
  let validate: ValidateFunction;
  try {
    validate = ajv.compile(prepared);
  } catch (err) {
    throw new ToolSchemaError(
      "schema_invalid",
      toolName,
      `Invalid inputSchema for tool "${toolName}": ${
        err instanceof Error ? err.message : String(err)
      }`,
      { cause: err },
    );
  }
  validatorCache.set(key, validate);
  while (validatorCache.size > MAX_CACHED_VALIDATORS) {
    const oldest = validatorCache.keys().next();
    if (oldest.done) break;
    validatorCache.delete(oldest.value);
  }
  return validate;
}

/**
 * One walk that both bounds the schema and copies it without `$id`/`$schema`.
 *
 * `$id` only names the schema in a registry this tool doesn't share any more,
 * and `$schema` names a DIALECT: an MCP server correctly advertising `2020-12`
 * (its own spec's dialect) made this draft-07 Ajv throw at compile time, and
 * the tool was silently dropped. Neither keyword changes how an instance
 * validates here — and both are stripped at EVERY level, because a NESTED `$id`
 * duplicated inside one document fails the compile with "resolves to more than
 * one schema" exactly like a top-level one used to.
 */
function prepareSchema(
  toolName: string,
  schema: object,
): Record<string, unknown> {
  let nodes = 0;
  const walk = (node: unknown, depth: number): unknown => {
    if (node === null || typeof node !== "object") return node;
    if (depth > MAX_SCHEMA_DEPTH) {
      throw new ToolSchemaError(
        "schema_too_large",
        toolName,
        `Invalid inputSchema for tool "${toolName}": nesting exceeds ${MAX_SCHEMA_DEPTH} levels.`,
      );
    }
    nodes += 1;
    if (nodes > MAX_SCHEMA_NODES) {
      throw new ToolSchemaError(
        "schema_too_large",
        toolName,
        `Invalid inputSchema for tool "${toolName}": exceeds ${MAX_SCHEMA_NODES} nodes.`,
      );
    }
    if (Array.isArray(node)) return node.map((item) => walk(item, depth + 1));
    const copy: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(
      node as Record<string, unknown>,
    )) {
      if (key === "$id" || key === "$schema") continue;
      copy[key] = walk(value, depth + 1);
    }
    return copy;
  };
  return walk(schema, 1) as Record<string, unknown>;
}
