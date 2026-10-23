import type {
  AiPromptContextBlock,
  RunContentSelection,
} from "@agentkit/contracts";
import type { PromptTemplateService } from "./template-service.js";
import type { SkillService } from "../skills/skill-service.js";
export interface RunContentContext {
  runId: string;
  chatId: string;
  signal?: AbortSignal;
}
export interface RunContentProvider {
  resolve(
    selection: RunContentSelection,
    ctx: RunContentContext,
  ): Promise<{
    blocks: AiPromptContextBlock[];
    messages: { role: "system" | "user" | "assistant"; content: string }[];
  }>;
}
export function createRunContentProvider(options: {
  templates?: PromptTemplateService;
  skills?: SkillService;
  authorizedConnectors(ctx: RunContentContext): Promise<readonly string[]>;
}): RunContentProvider {
  return {
    async resolve(selection, ctx) {
      ctx.signal?.throwIfAborted();
      const messages: {
        role: "system" | "user" | "assistant";
        content: string;
      }[] = [];
      const blocks: AiPromptContextBlock[] = [];
      if (selection.template) {
        if (!options.templates)
          throw new Error("Prompt templates are not configured.");
        messages.push(
          ...(await options.templates.render(selection.template)).messages,
        );
      }
      if (selection.skills?.length) {
        if (!options.skills) throw new Error("Skills are not configured.");
        if (
          new Set(selection.skills.map((skill) => skill.id)).size !==
            selection.skills.length ||
          selection.skills.length > 20
        )
          throw new Error("Invalid skill selection.");
        const connectors = await options.authorizedConnectors(ctx);
        for (const skill of selection.skills) {
          ctx.signal?.throwIfAborted();
          blocks.push(
            (await options.skills.activate(skill, connectors, ctx.signal))
              .block,
          );
        }
      }
      ctx.signal?.throwIfAborted();
      return { blocks, messages };
    },
  };
}
