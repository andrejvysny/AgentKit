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
  /**
   * Present, a Skill that fails to activate is reported here and skipped
   * instead of failing the whole resolve. Agent sessions carry Skills over
   * from the descriptor, where a since-edited Skill must not end the turn.
   */
  onSkillUnavailable?(skillId: string, reason: unknown): void;
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
        if (!options.skills) {
          if (ctx.onSkillUnavailable === undefined)
            throw new Error("Skills are not configured.");
          for (const skill of selection.skills)
            ctx.onSkillUnavailable(
              skill.id,
              new Error("Skills are not configured."),
            );
          return { blocks, messages };
        }
        if (
          new Set(selection.skills.map((skill) => skill.id)).size !==
            selection.skills.length ||
          selection.skills.length > 20
        )
          throw new Error("Invalid skill selection.");
        const connectors = await options.authorizedConnectors(ctx);
        for (const skill of selection.skills) {
          ctx.signal?.throwIfAborted();
          try {
            blocks.push(
              (await options.skills.activate(skill, connectors, ctx.signal))
                .block,
            );
          } catch (error) {
            if (ctx.onSkillUnavailable === undefined || ctx.signal?.aborted)
              throw error;
            ctx.onSkillUnavailable(skill.id, error);
          }
        }
      }
      ctx.signal?.throwIfAborted();
      return { blocks, messages };
    },
  };
}
