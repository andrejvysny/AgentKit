import type { AiPromptContextBlock, AiPromptPreset } from "./types.js";

export interface ComposeSystemPromptInput {
  preset: AiPromptPreset;
  blocks?: AiPromptContextBlock[];
  toolInstructions?: string;
}

export function composeSystemPrompt(input: ComposeSystemPromptInput): string {
  const parts: string[] = [input.preset.systemText.trim()];

  if (input.blocks && input.blocks.length > 0) {
    const ordered = [...input.blocks].sort((a, b) => a.priority - b.priority);
    for (const block of ordered) {
      parts.push(`\n## ${block.title}\n${block.content.trim()}`);
    }
  }

  if (input.toolInstructions && input.toolInstructions.trim().length > 0) {
    parts.push(`\n## Tool rules\n${input.toolInstructions.trim()}`);
  }

  return parts.join("\n");
}
