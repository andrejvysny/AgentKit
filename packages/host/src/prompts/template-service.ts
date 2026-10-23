import type {
  PromptTemplate,
  PromptTemplateSelection,
} from "@agentkit/contracts";
import { renderPromptTemplate, validatePromptTemplate } from "@agentkit/core";

/** Revisions are immutable. Publishing a change requires a new revision. */
export interface PromptTemplateStore {
  put(template: PromptTemplate): Promise<void>;
  get(id: string, revision: string): Promise<PromptTemplate | null>;
  list(): Promise<PromptTemplate[]>;
  delete(id: string, revision: string): Promise<void>;
}
export class PromptTemplateService {
  constructor(private readonly store: PromptTemplateStore) {}
  async publish(template: PromptTemplate) {
    validatePromptTemplate(template);
    await this.store.put(structuredClone(template));
    return structuredClone(template);
  }
  list() {
    return this.store.list();
  }
  get(id: string, revision: string) {
    return this.store.get(id, revision);
  }
  remove(id: string, revision: string) {
    return this.store.delete(id, revision);
  }
  async render(selection: PromptTemplateSelection) {
    const template = await this.store.get(selection.id, selection.revision);
    if (!template) throw new Error("Prompt template revision not found.");
    return renderPromptTemplate(template, selection);
  }
  preview(template: PromptTemplate, args: Record<string, unknown>) {
    return renderPromptTemplate(template, {
      id: template.id,
      revision: template.revision,
      arguments: args,
    });
  }
}
