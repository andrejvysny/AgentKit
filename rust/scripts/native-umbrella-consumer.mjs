import {
  AiToolRegistry,
  ExecutionBudget,
  resolveToolLimits,
  runChat,
} from "agentkit/core";
import { MockProviderClient } from "agentkit/testing";

const mode = process.argv[2];
const provider = new MockProviderClient();
const registry = new AiToolRegistry();
const input = {
  client: provider,
  registry,
  model: "test-model",
  messages: [],
  limits: resolveToolLimits({ preference: "small" }),
};
function assert(condition, message) {
  if (!condition) throw new Error(message);
}
async function collect(iterator) {
  const events = [];
  for (;;) {
    const item = await iterator.next();
    if (item.done) return { events, result: item.value };
    events.push(item.value);
  }
}

if (mode === "native") {
  let calls = 0;
  registry.register({
    definition: {
      name: "lookup",
      version: "1",
      effect: "read",
      capability: "test",
      description: "fixture",
      inputSchema: { type: "object" },
    },
    async execute(context) {
      calls++;
      return {
        ok: true,
        data: { found: true },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  });
  provider.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "c1",
          name: "lookup",
          argumentsJson: "{}",
        },
      ],
    },
    { steps: [{ kind: "text", content: "native result" }] },
  ]);
  const { events, result } = await collect(runChat(input));
  assert(
    result.terminal === "completed" && result.iterations === 2,
    "native run did not complete in two iterations",
  );
  assert(
    calls === 1 && events.some((event) => event.type === "run.tool.succeeded"),
    "JS tool did not execute through native loop",
  );
  const budget = new ExecutionBudget({ providerRequests: 0 });
  try {
    const bounded = await collect(
      runChat({ ...input, executionBudget: budget }),
    );
    assert(
      bounded.result.terminal === "failed" &&
        bounded.events.some(
          (event) =>
            event.type === "run.failed" &&
            event.data.errorCode === "execution_budget_exhausted",
        ),
      "native backend did not enforce the external execution budget",
    );
  } finally {
    budget.dispose();
  }
} else if (mode === "ts") {
  provider.setScript([
    { steps: [{ kind: "text", content: "typescript result" }] },
  ]);
  const { result } = await collect(runChat(input));
  assert(
    result.terminal === "completed",
    "default TypeScript backend failed without addon",
  );
} else if (mode === "missing") {
  try {
    await collect(runChat(input));
    throw new Error("native backend unexpectedly ran without addon");
  } catch (error) {
    assert(
      String(error).includes("native binding failed to load"),
      `wrong missing addon error: ${String(error)}`,
    );
  }
} else throw new Error(`unknown consumer mode: ${mode}`);

console.log(
  `umbrella-native-consumer-ok mode=${mode} runtime=${process.versions.bun ? `bun ${process.versions.bun}` : `node ${process.version}`}`,
);
