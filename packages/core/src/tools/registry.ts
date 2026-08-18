import Ajv, { type ValidateFunction } from "ajv";
import { TOOL_NAME_PATTERN, type AiTool } from "./types.js";
import { mapValidatorErrors, type ValidationError } from "./validation.js";

export class AiToolRegistry {
  private readonly tools = new Map<string, AiTool>();
  /** Per-tool inputSchema validators, compiled once at registration. */
  private readonly validators = new Map<string, ValidateFunction>();
  /**
   * Shared Ajv instance. `strict: false` because tool inputSchemas are authored
   * loosely (unknown keywords tolerated); `allErrors: true` mirrors validation.ts
   * so callers see every problem.
   */
  private readonly ajv = new Ajv({ allErrors: true, strict: false });

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
    const validate = this.ajv.compile(tool.definition.inputSchema);
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
