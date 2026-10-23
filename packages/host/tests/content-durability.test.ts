import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ResponsesClient } from "@agentkit/core";
import type { RunContentSelection, PromptTemplate } from "@agentkit/contracts";
import {
  TurnRunner,
  PromptTemplateService,
  SkillService,
  bundledSkillSource,
  createRunContentProvider,
} from "../src/index.js";
import {
  SqliteAssistantStore,
  SharedSqlitePromptTemplateStore,
} from "../../adapters-sqlite/src/index.js";
import { createTestClock } from "./fakes.js";
import {
  execute,
  fixture,
  response,
  outputText,
  eventStream,
  reasoning,
  toolCall,
} from "./provider-continuation-helpers.js";

const template: PromptTemplate = {
  id: "review",
  revision: "1",
  description: "Review selected project",
  argumentsSchema: {
    type: "object",
    properties: { project: { type: "string" } },
    required: ["project"],
  },
  messages: [
    { role: "user", content: "TEMPLATE_USER {{project}}" },
    { role: "assistant", content: "TEMPLATE_ASSISTANT {{project}}" },
  ],
};
const skill =
  "---\nname: project-review\ndescription: Review project\n---\nSELECTED_SKILL";

async function contentServices(store: SqliteAssistantStore) {
  const templates = new PromptTemplateService(
    new SharedSqlitePromptTemplateStore(store.database),
  );
  const skills = new SkillService([
    bundledSkillSource("app", { review: { "SKILL.md": skill } }),
  ]);
  const selected = (await skills.catalog())[0]!;
  return {
    templates,
    selection: {
      template: {
        id: template.id,
        revision: template.revision,
        arguments: { project: "AgentKit" },
      },
      skills: [{ id: selected.id, revision: selected.revision }],
    } satisfies RunContentSelection,
    runContent: createRunContentProvider({
      templates,
      skills,
      authorizedConnectors: async () => [],
    }),
  };
}

test("file SQLite reopen preserves pending selection and injects once through tool replay", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-content-"));
  const path = join(directory, "state.sqlite");
  const clock = createTestClock();
  let store = new SqliteAssistantStore(path, { clock });
  try {
    const content = await contentServices(store);
    await content.templates.publish(template);
    const before = await fixture({
      store,
      overrides: { runContent: content.runContent },
    });
    const selection = structuredClone(content.selection);
    const submitted = await before.runner.submitMessage({
      chatId: before.chatId,
      content: "Review",
      contentSelection: selection,
    });
    selection.template!.revision = "changed-after-submit";
    store.close();
    store = new SqliteAssistantStore(path, { clock });
    const reopenedContent = await contentServices(store);
    await reopenedContent.templates.publish({
      ...template,
      revision: "2",
      messages: [{ role: "user", content: "NEW_REVISION" }],
    });
    expect(
      (await store.tasks.getTask(submitted.runId))?.payload.contentSelection,
    ).toEqual(content.selection);
    const resumed = await fixture({
      store,
      existingChat: before.chatId,
      overrides: { runContent: reopenedContent.runContent },
    });
    await execute(resumed.runner, store, submitted.runId);
    expect((await store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    expect(resumed.sent).toHaveLength(2);
    for (const request of resumed.sent) {
      const body = JSON.stringify(request.body);
      expect(body.split("TEMPLATE_USER AgentKit")).toHaveLength(2);
      expect(body.split("TEMPLATE_ASSISTANT AgentKit")).toHaveLength(2);
      expect(body.split("SELECTED_SKILL")).toHaveLength(2);
      expect(body).not.toContain("NEW_REVISION");
    }
    expect(resumed.executed()).toBe(1);
  } finally {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});

test("Responses next turn after selected content survives reopen without reselecting", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-content-history-"));
  const path = join(directory, "state.sqlite");
  const clock = createTestClock();
  let store = new SqliteAssistantStore(path, { clock });
  try {
    const content = await contentServices(store);
    await content.templates.publish(template);
    const first = await fixture({
      store,
      overrides: { runContent: content.runContent },
      output: () => [response([outputText("Reviewed")])],
    });
    const submitted = await first.runner.submitMessage({
      chatId: first.chatId,
      content: "Review",
      contentSelection: content.selection,
    });
    await execute(first.runner, store, submitted.runId);
    store.close();
    store = new SqliteAssistantStore(path, { clock });
    const resumed = await fixture({
      store,
      existingChat: first.chatId,
      output: () => [response([outputText("Continued")])],
    });
    const next = await resumed.runner.submitMessage({
      chatId: first.chatId,
      content: "Continue",
    });
    await execute(resumed.runner, store, next.runId);
    expect(
      (await store.tasks.listEvents(next.runId)).filter(
        (event) => event.type === "run.failed",
      ),
    ).toEqual([]);
    expect((await store.tasks.getTask(next.runId))?.status).toBe("completed");
    expect(resumed.sent).toHaveLength(1);
    expect(JSON.stringify(resumed.sent[0]?.body)).not.toContain(
      "SELECTED_SKILL",
    );
    const current = await contentServices(store);
    await current.templates.publish({
      ...template,
      revision: "2",
      messages: [{ role: "user", content: "NEW_TEMPLATE" }],
    });
    const runner = new TurnRunner({
      ...resumed.deps,
      runContent: current.runContent,
    });
    const third = await runner.submitMessage({
      chatId: first.chatId,
      content: "Review again",
      contentSelection: {
        ...current.selection,
        template: { ...current.selection.template, revision: "2" },
      },
    });
    await execute(runner, store, third.runId);
    expect((await store.tasks.getTask(third.runId))?.status).toBe("completed");
    const thirdBody = JSON.stringify(resumed.sent[1]?.body);
    expect(thirdBody.split("NEW_TEMPLATE")).toHaveLength(2);
    expect(thirdBody.split("TEMPLATE_USER AgentKit")).toHaveLength(2);
    expect(thirdBody.split("SELECTED_SKILL")).toHaveLength(2);
  } finally {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});

test("invalid content fails before provider construction, discovery or tool execution", async () => {
  for (const invalid of [
    "missing",
    "unknown",
    "skill-drift",
    "connector",
  ] as const) {
    const store = new SqliteAssistantStore(":memory:");
    try {
      const content = await contentServices(store);
      await content.templates.publish(template);
      const selection: RunContentSelection = structuredClone(content.selection);
      if (invalid === "missing") selection.template!.arguments = {};
      if (invalid === "unknown") selection.template!.arguments.extra = true;
      let selectedSkill = skill;
      const skills = new SkillService([
        {
          id: "app",
          list: async () => ["review"],
          read: async () => selectedSkill,
        },
      ]);
      if (invalid === "connector") {
        selectedSkill = skill.replace(
          "---\nSELECTED_SKILL",
          "metadata:\n  requires-connectors: github\n---\nSELECTED_SKILL",
        );
      }
      selection.skills = (await skills.catalog()).map(({ id, revision }) => ({
        id,
        revision,
      }));
      const runContent = createRunContentProvider({
        templates: content.templates,
        skills,
        authorizedConnectors: async () => [],
      });
      const environment = await fixture({ store });
      let constructed = 0;
      let discovered = 0;
      const runner = new TurnRunner({
        ...environment.deps,
        runContent,
        providerFactory: (config) => {
          constructed++;
          return environment.deps.providerFactory(config);
        },
        contributors: [
          {
            namespace: "audit",
            contribute: async () => {
              discovered++;
              return [];
            },
          },
        ],
      });
      const submitted = await runner.submitMessage({
        chatId: environment.chatId,
        content: "Review",
        contentSelection: selection,
      });
      if (invalid === "skill-drift") selectedSkill += " changed";
      await expect(execute(runner, store, submitted.runId)).rejects.toThrow(
        invalid === "missing" || invalid === "unknown"
          ? "Invalid template arguments"
          : invalid === "skill-drift"
            ? "revision changed"
            : "authorized connector",
      );
      expect(constructed).toBe(0);
      expect(discovered).toBe(0);
      expect(environment.sent).toHaveLength(0);
      expect(environment.executed()).toBe(0);
      expect((await store.tasks.getTask(submitted.runId))?.status).toBe(
        "failed",
      );
    } finally {
      store.close();
    }
  }
});

test("abandoned selected run resumes after SQLite reopen without duplicate context or tool effects", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-content-resume-"));
  const path = join(directory, "state.sqlite");
  const clock = createTestClock();
  let store = new SqliteAssistantStore(path, { clock });
  try {
    const content = await contentServices(store);
    await content.templates.publish(template);
    const environment = await fixture({
      store,
      overrides: { runContent: content.runContent },
    });
    const blocked = Promise.withResolvers<void>();
    let requests = 0;
    const client = new ResponsesClient({
      id: "provider",
      transport: {
        identity: {
          providerId: "provider",
          protocol: "responses",
          connectionId: "account",
          generation: 1,
        },
        authKind: "chatgpt-account",
        async request(input) {
          requests++;
          if (requests === 1)
            return eventStream([
              response([
                reasoning,
                outputText("Checking", "intro"),
                toolCall(),
              ]),
            ]);
          blocked.resolve();
          return new Promise<Response>((_resolve, reject) => {
            input.signal?.addEventListener(
              "abort",
              () => reject(new DOMException("Abandoned", "AbortError")),
              { once: true },
            );
          });
        },
      },
    });
    const runner = new TurnRunner({
      ...environment.deps,
      providerFactory: () => client,
    });
    const submitted = await runner.submitMessage({
      chatId: environment.chatId,
      content: "Review",
      contentSelection: content.selection,
    });
    const claim = (await store.tasks.claimNext({
      ownerId: "old-worker",
      now: clock.now(),
      scopesBusy: [],
    }))!;
    const controller = new AbortController();
    const active = runner.executeTask({
      task: claim.task,
      attemptId: claim.attempt.attemptId,
      leaseToken: claim.lease.leaseToken,
      signal: controller.signal,
    });
    await blocked.promise;
    expect(environment.executed()).toBe(1);
    await store.tasks.endAttempt({
      attemptId: claim.attempt.attemptId,
      status: "abandoned",
      leaseToken: claim.lease.leaseToken,
    });
    await store.tasks.transitionTask(
      submitted.runId,
      ["running"],
      "interrupted",
      {},
      { leaseToken: claim.lease.leaseToken },
    );
    await store.tasks.releaseLease(claim.lease.leaseToken);
    await store.tasks.transitionTask(
      submitted.runId,
      ["interrupted"],
      "queued",
    );
    controller.abort();
    await active;
    store.close();
    store = new SqliteAssistantStore(path, { clock });
    const reopenedContent = await contentServices(store);
    const resumed = await fixture({
      store,
      existingChat: environment.chatId,
      overrides: { runContent: reopenedContent.runContent },
      output: () => [response([outputText("Resumed")])],
    });
    await execute(resumed.runner, store, submitted.runId);
    expect((await store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    expect((await store.tasks.getTask(submitted.runId))?.attemptCount).toBe(2);
    expect(resumed.executed()).toBe(0);
    expect(resumed.sent).toHaveLength(1);
    const body = JSON.stringify(resumed.sent[0]?.body);
    for (const marker of [
      "TEMPLATE_USER AgentKit",
      "TEMPLATE_ASSISTANT AgentKit",
      "SELECTED_SKILL",
      "private-encrypted-state",
    ])
      expect(body.split(marker)).toHaveLength(2);
    expect(
      (
        (resumed.sent[0]?.body?.input ?? []) as Record<string, unknown>[]
      ).filter((item) => item.type === "function_call_output"),
    ).toHaveLength(1);
  } finally {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});

test("pending pinned template fails missing after durable deletion instead of using replacement bytes", async () => {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-content-deleted-"));
  const path = join(directory, "state.sqlite");
  const clock = createTestClock();
  let store = new SqliteAssistantStore(path, { clock });
  try {
    const content = await contentServices(store);
    await content.templates.publish(template);
    const before = await fixture({ store });
    const submitted = await before.runner.submitMessage({
      chatId: before.chatId,
      content: "Review",
      contentSelection: content.selection,
    });
    await content.templates.remove(template.id, template.revision);
    store.close();
    store = new SqliteAssistantStore(path, { clock });
    const current = await contentServices(store);
    await expect(
      current.templates.publish({
        ...template,
        messages: [{ role: "user", content: "replacement" }],
      }),
    ).rejects.toThrow();
    const resumed = await fixture({
      store,
      existingChat: before.chatId,
      overrides: { runContent: current.runContent },
    });
    await expect(
      execute(resumed.runner, store, submitted.runId),
    ).rejects.toThrow("revision not found");
    expect(resumed.sent).toHaveLength(0);
    expect((await store.tasks.getTask(submitted.runId))?.status).toBe("failed");
  } finally {
    store.close();
    rmSync(directory, { recursive: true, force: true });
  }
});
