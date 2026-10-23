import type { PromptTemplate } from "@agentkit/contracts";
import type { PromptTemplateStore } from "@agentkit/host";
export class MemoryPromptTemplateStore implements PromptTemplateStore {
  private readonly templates = new Map<string, PromptTemplate>();
  private key(id: string, revision: string) {
    return JSON.stringify([id, revision]);
  }
  async put(template: PromptTemplate) {
    const key = this.key(template.id, template.revision);
    if (this.templates.has(key))
      throw new Error("Template revision already exists.");
    this.templates.set(key, structuredClone(template));
  }
  async get(id: string, revision: string) {
    return structuredClone(this.templates.get(this.key(id, revision)) ?? null);
  }
  async list() {
    return structuredClone([...this.templates.values()]);
  }
  async delete(id: string, revision: string) {
    this.templates.delete(this.key(id, revision));
  }
}
