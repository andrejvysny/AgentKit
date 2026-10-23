import type { PromptTemplateService } from "@agentkit/host";
import type { McpPromptSource, McpSessionScope } from "./types.js";

/** Explicitly selected immutable versions only; MCP prompt roles remain user/assistant. */
export function templatePromptSource(
  service: PromptTemplateService,
  selected: (
    scope: McpSessionScope,
  ) => Promise<{ id: string; revision: string }[]>,
): McpPromptSource {
  return {
    async list(scope) {
      const prompts = [];
      for (const item of await selected(scope)) {
        scope.signal?.throwIfAborted();
        const template = await service.get(item.id, item.revision);
        if (
          !template ||
          template.messages.some((message) => message.role === "system")
        )
          continue;
        prompts.push({
          name: `${encodeURIComponent(item.id)}@${encodeURIComponent(item.revision)}`,
          description: template.description,
          arguments: Object.keys(template.argumentsSchema.properties ?? {}).map(
            (name) => ({
              name,
              required:
                template.argumentsSchema.required?.includes(name) ?? false,
            }),
          ),
        });
      }
      return prompts;
    },
    async get(name, args, scope) {
      const item = (await selected(scope)).find(
        (item) =>
          `${encodeURIComponent(item.id)}@${encodeURIComponent(item.revision)}` ===
          name,
      );
      if (!item) throw new Error("Prompt selection changed.");
      const result = await service.render({ ...item, arguments: args });
      if (result.messages.some((message) => message.role === "system"))
        throw new Error("MCP prompts cannot carry system roles.");
      return {
        messages: result.messages.map((message) => ({
          role: message.role as "user" | "assistant",
          content: { type: "text" as const, text: message.content },
        })),
      };
    },
  };
}
