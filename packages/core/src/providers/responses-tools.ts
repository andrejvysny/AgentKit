import Ajv from "ajv";
import {
  TOOL_NAME_PATTERN,
  type AiToolDefinition,
  type AiToolCall,
} from "@agentkit/contracts";
import { boundedJson, object, responsesError } from "./responses-types.js";

const schemaValidator = new Ajv({ strict: false, logger: false });

export const RESPONSES_TOOL_NAMESPACE = "agentkit";

/** Names stay verbatim inside a fixed namespace; no lossy replacement or hashing. */
export function responsesTools(
  tools: AiToolDefinition[] = [],
): Record<string, unknown>[] {
  const seen = new Set<string>();
  const functions = tools.map((tool) => {
    if (object(tool).type !== undefined) responsesError("unsupported_tool");
    if (
      !TOOL_NAME_PATTERN.test(tool.name) ||
      tool.name.length > 64 ||
      seen.has(tool.name)
    ) {
      return responsesError("unsupported_tool");
    }
    seen.add(tool.name);
    validateSchema(object(tool.inputSchema));
    if (!schemaValidator.validateSchema(tool.inputSchema))
      responsesError("unsupported_tool_schema");
    if (tool.inputSchema.type !== "object")
      responsesError("unsupported_tool_schema");
    return {
      type: "function",
      name: tool.name,
      description: tool.description,
      parameters: JSON.parse(boundedJson(tool.inputSchema)) as unknown,
      strict: false,
    };
  });
  return functions.length
    ? [
        {
          type: "namespace",
          name: RESPONSES_TOOL_NAMESPACE,
          description: "Tools supplied by the application.",
          tools: functions,
        },
      ]
    : [];
}

export function responseToolCall(
  item: Record<string, unknown>,
  tools: AiToolDefinition[] = [],
): AiToolCall {
  if (
    item.type !== "function_call" ||
    item.namespace !== RESPONSES_TOOL_NAMESPACE ||
    typeof item.name !== "string" ||
    !tools.some((tool) => tool.name === item.name) ||
    typeof item.call_id !== "string" ||
    !item.call_id ||
    typeof item.arguments !== "string"
  ) {
    return responsesError("unknown_tool");
  }
  try {
    JSON.parse(item.arguments);
  } catch {
    responsesError("malformed_tool_arguments");
  }
  return { id: item.call_id, name: item.name, argumentsJson: item.arguments };
}

const SCHEMA_KEYS = new Set([
  "type",
  "description",
  "properties",
  "required",
  "items",
  "enum",
  "additionalProperties",
  "default",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "minLength",
  "maxLength",
  "minItems",
  "maxItems",
  "anyOf",
  "oneOf",
  "allOf",
  "const",
  "title",
]);

function validateSchema(schema: Record<string, unknown>, depth = 0): void {
  if (depth > 32) responsesError("unsupported_tool_schema");
  for (const [key, value] of Object.entries(schema)) {
    if (!SCHEMA_KEYS.has(key)) responsesError("unsupported_tool_schema");
    if (key === "properties") {
      for (const child of Object.values(object(value)))
        validateSchema(object(child), depth + 1);
    } else if (
      key === "items" ||
      (key === "additionalProperties" && typeof value !== "boolean")
    ) {
      validateSchema(object(value), depth + 1);
    } else if (key === "anyOf" || key === "oneOf" || key === "allOf") {
      if (!Array.isArray(value) || !value.length)
        responsesError("unsupported_tool_schema");
      for (const child of value) validateSchema(object(child), depth + 1);
    }
  }
  boundedJson(schema);
}
