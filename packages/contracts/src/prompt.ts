export interface AiPromptPreset {
  id: string;
  label: string;
  description: string;
  systemText: string;
}

export interface AiPromptContextBlock {
  id: string;
  title: string;
  content: string;
  priority: number;
}
