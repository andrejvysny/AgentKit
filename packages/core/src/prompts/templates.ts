import type {
  PromptTemplate,
  PromptTemplateSelection,
} from "@agentkit/contracts";
import { AiToolRegistry } from "../tools/registry.js";

/** Stable JSON rendering: no evaluation, helpers, property access or recursive expansion. */
function stable(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stable).join(",")}]`;
  if (value && typeof value === "object")
    return `{${Object.keys(value)
      .sort()
      .map(
        (key) =>
          `${JSON.stringify(key)}:${stable((value as Record<string, unknown>)[key])}`,
      )
      .join(",")}}`;
  const result = JSON.stringify(value);
  if (result === undefined) throw new Error("Template argument is not JSON.");
  return result;
}
export function validatePromptTemplate(template: PromptTemplate): void {
  if (
    !template.id ||
    !template.revision ||
    template.messages.length === 0 ||
    template.messages.length > 100
  )
    throw new Error("Invalid template identity or message count.");
  if (template.argumentsSchema.type !== "object")
    throw new Error("Template arguments must be an object schema.");
  const properties = template.argumentsSchema.properties ?? {};
  for (const message of template.messages) {
    if (
      !["system", "user", "assistant"].includes(message.role) ||
      typeof message.content !== "string" ||
      message.content.length > 1_000_000
    )
      throw new Error("Invalid template message.");
    for (const match of message.content.matchAll(
      /\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}/g,
    ))
      if (!Object.hasOwn(properties, match[1]!))
        throw new Error(`Unknown template variable: ${match[1]}`);
    if (
      message.content
        .replace(/\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}/g, "")
        .includes("{{")
    )
      throw new Error("Unsupported template expression.");
  }
  validator(template);
}
function validator(template: PromptTemplate) {
  const registry = new AiToolRegistry();
  registry.register({
    definition: {
      name: "template",
      version: template.revision,
      effect: "read",
      capability: "prompt.render",
      description: template.description,
      inputSchema: { ...template.argumentsSchema, additionalProperties: false },
    },
    execute: async () => {
      throw new Error("validation only");
    },
  });
  return registry;
}
export function renderPromptTemplate(
  template: PromptTemplate,
  selection: PromptTemplateSelection,
) {
  if (template.id !== selection.id || template.revision !== selection.revision)
    throw new Error("Template revision mismatch.");
  validatePromptTemplate(template);
  // JSON round-trip rejects cycles, undefined and host objects at the boundary.
  const args = structuredClone(selection.arguments);
  stable(args);
  const errors = validator(template).validateInput("template", args);
  if (errors.length)
    throw new Error(
      `Invalid template arguments: ${errors.map((e) => `${e.path}: ${e.message}`).join("; ")}`,
    );
  return {
    provenance: {
      id: template.id,
      revision: template.revision,
      origin: template.origin,
    },
    messages: template.messages.map((message) => ({
      ...message,
      content: message.content.replace(
        /\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}/g,
        (_, key: string) => {
          if (!Object.hasOwn(args, key))
            throw new Error(`Missing template argument: ${key}`);
          const value = args[key];
          return typeof value === "string" ? value : stable(value);
        },
      ),
    })),
  };
}
