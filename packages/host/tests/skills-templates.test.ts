import { expect, test } from "bun:test";
import {
  SkillService,
  bundledSkillSource,
  parseSkillDocument,
  PromptTemplateService,
  createRunContentProvider,
} from "../src/index.js";
import { MemoryPromptTemplateStore } from "../../adapters-memory/src/memory-prompt-template-store.js";
import type { PromptTemplate } from "@agentkit/contracts";
const template: PromptTemplate = {
  id: "review",
  revision: "1",
  description: "Review a project",
  argumentsSchema: {
    type: "object",
    properties: { project: { type: "string" } },
    required: ["project"],
  },
  messages: [
    { role: "user", content: "Review {{project}}" },
    { role: "assistant", content: "I will review {{ project }}." },
  ],
};
const skill =
  "---\nname: review-project\ndescription: >\n  Review a project\n  carefully.\nmetadata:\n  requires-connectors: github\n---\nInspect the selected repository. Read references/checklist.md when needed.";

test("template render preserves roles, rejects missing/unknown inputs and pins immutable versions", async () => {
  const service = new PromptTemplateService(new MemoryPromptTemplateStore());
  await service.publish(template);
  const first = await service.render({
    id: "review",
    revision: "1",
    arguments: { project: "{{not_code}}" },
  });
  expect(first.messages).toEqual([
    { role: "user", content: "Review {{not_code}}" },
    { role: "assistant", content: "I will review {{not_code}}." },
  ]);
  await expect(
    service.render({ id: "review", revision: "1", arguments: {} }),
  ).rejects.toThrow("Invalid template arguments");
  await expect(
    service.render({
      id: "review",
      revision: "1",
      arguments: { project: "x", unknown: true },
    }),
  ).rejects.toThrow("Invalid template arguments");
  await expect(service.publish(template)).rejects.toThrow("already exists");
  await service.publish({
    ...template,
    revision: "2",
    messages: [{ role: "user", content: "New {{project}}" }],
  });
  expect(
    (
      await service.render({
        id: "review",
        revision: "1",
        arguments: { project: "old" },
      })
    ).messages[0]?.content,
  ).toBe("Review old");
});
test("template rejects code-like expressions and undeclared variables", async () => {
  const service = new PromptTemplateService(new MemoryPromptTemplateStore());
  await expect(
    service.publish({
      ...template,
      messages: [{ role: "user", content: "{{project.toUpperCase()}}" }],
    }),
  ).rejects.toThrow("Unsupported template expression");
  await expect(
    service.publish({
      ...template,
      messages: [{ role: "user", content: "{{missing}}" }],
    }),
  ).rejects.toThrow("Unknown template variable");
});
test("skill catalog loads metadata; explicit activation pins instructions and authorized dependencies", async () => {
  const resources: unknown[] = [];
  const service = new SkillService(
    [
      bundledSkillSource("app", {
        review: {
          "SKILL.md": skill,
          "references/checklist.md": "Check tests.",
        },
      }),
    ],
    async (item) => {
      resources.push(item);
    },
  );
  const [entry] = await service.catalog();
  expect(entry).toMatchObject({
    id: "app:review",
    name: "review-project",
    description: "Review a project carefully.",
    connectorRequirements: ["github"],
  });
  expect(JSON.stringify(entry)).not.toContain(
    "Inspect the selected repository",
  );
  await expect(service.activate(entry!, [])).rejects.toThrow(
    "authorized connector",
  );
  const active = await service.activate(entry!, ["github"]);
  expect(active.block.content).toContain("Inspect the selected repository");
  expect((await active.readReference("references/checklist.md")).text).toBe(
    "Check tests.",
  );
  expect(resources).toHaveLength(1);
  await expect(active.readReference("../secret")).rejects.toThrow(
    "inside its root",
  );
  await expect(
    service.activate({ ...entry!, revision: "stale" }, ["github"]),
  ).rejects.toThrow("revision changed");
});
test("skill parsing refuses executable YAML and duplicate metadata", () => {
  expect(() =>
    parseSkillDocument(
      "---\nname: test\ndescription: !!js/function x\n---\nbody",
    ),
  ).toThrow("Unsupported YAML");
  expect(() =>
    parseSkillDocument(
      "---\nname: test\nname: other\ndescription: test\n---\nbody",
    ),
  ).toThrow("Duplicate");
});
test("run content keeps template roles and loads each selected skill once", async () => {
  const templates = new PromptTemplateService(new MemoryPromptTemplateStore());
  await templates.publish(template);
  const skills = new SkillService([
    bundledSkillSource("app", { review: { "SKILL.md": skill } }),
  ]);
  const [selected] = await skills.catalog();
  const provider = createRunContentProvider({
    templates,
    skills,
    authorizedConnectors: async () => ["github"],
  });
  const resolved = await provider.resolve(
    {
      template: {
        id: "review",
        revision: "1",
        arguments: { project: "AgentKit" },
      },
      skills: [selected!],
    },
    { runId: "run", chatId: "chat" },
  );
  expect(resolved.messages).toHaveLength(2);
  expect(resolved.messages[0]?.role).toBe("user");
  expect(resolved.blocks).toHaveLength(1);
});
