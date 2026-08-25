/**
 * The vertical slice: every layer of AgentKit, wired the way a host wires it,
 * running over the durable sqlite store with only the model faked.
 *
 * `task-runner-integration.test.ts` proves the seam between the queue and the
 * turn worker. This file is the whole stack — provider → run loop → tool
 * registry → write tool → proposal pipeline → applier — and it exists because
 * the properties that matter most are not properties of any one component:
 *
 * - a submitted message becomes a durable run, a placeholder, and eventually an
 *   answer, with ONE unbroken event stream nobody had to coordinate;
 * - a write the model asks for is STAGED, not performed, unless a policy said
 *   otherwise — and what the model is told about it is the slim envelope, never
 *   the host's payload;
 * - "apply" is safe to say twice, and safe to be interrupted in the middle of.
 *
 * It doubles as the worked example of embedding the framework: {@link embedSlice}
 * is the wiring, and nothing below it reaches past a public port.
 *
 * DETERMINISM: time and identity are injected (a frozen {@link TestClock} and
 * counter-based ids), the provider is scripted, and the only real waiting is the
 * harness's 1ms `waitFor` polling of the dispatch loop.
 */
import { describe, expect, it } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiChatMessage,
  type AiRunEvent,
  type AiToolDefinition,
  type AiToolEnvelope,
} from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  ProposalService,
  SessionWritePolicy,
  TurnRunner,
  createDispatchingWorker,
  createProposalBuilderTool,
  type ApplyOutcome,
  type ApplyProposalInput,
  type IdGenerator,
  type MessageRecord,
  type ProposalApplier,
  type ProposalRecord,
  type SubmitMessageResult,
  type ToolSetContributor,
  type WriteToolModelData,
} from "@agentkit/host";
import { HangingProviderClient, MockProviderClient } from "@agentkit/testing";
import { SingleProcessTaskRunner, SqliteAssistantStore } from "../src/index.js";
import {
  createTestClock,
  waitFor,
  type TestClock,
} from "./support/task-runner-harness.js";

// ---------------------------------------------------------------------------
// The host's domain, such as it is: one chat, one scope, one write tool.
// ---------------------------------------------------------------------------

const CHAT_ID = "chat-e2e";
/**
 * The scope the directly-staged proposals below use — a second namespace, so
 * the crash-reconciliation scenario cannot collide with what the tool stages.
 *
 * The TOOL's scope is not this constant: it reads `ctx.scopeId`, which the run
 * carries (here the chat, since `submitMessage` scopes a turn on its chat).
 */
const SCOPE_KEY = "scope-A";
/** `<verb>_<primaryKey>_<scopeId>`, as the model is told to derive it. */
const ACTION_ID = "append_note-1_scope-A";
const NOTE_TEXT = "hello";
const TOOL_CALL_ID = "call-notes-1";
const PROPOSAL_KIND = "notes.append";
const FINAL_ANSWER = "Noted: hello.";

interface NotesAppendInput {
  action_id?: string;
  text: string;
}

const NOTES_APPEND: AiToolDefinition = {
  name: "notes_append",
  version: "1.0.0",
  effect: "write",
  capability: "notes.write",
  description: "Append a note to the shared notebook.",
  inputSchema: {
    type: "object",
    properties: {
      action_id: { type: "string" },
      text: { type: "string" },
    },
    required: ["action_id", "text"],
  },
};

/** Counter-based ids, so an assertion can name the id it expects. */
function createSequentialIds(): IdGenerator {
  const counters = new Map<string, number>();
  const next = (kind: string): string => {
    const n = (counters.get(kind) ?? 0) + 1;
    counters.set(kind, n);
    return `${kind}-${n}`;
  };
  return {
    taskId: () => next("task"),
    attemptId: () => next("att"),
    eventId: () => next("evt"),
    proposalId: () => next("prp"),
    operationId: () => next("op"),
    messageId: () => next("msg"),
  };
}

/**
 * The only component in this file that changes the world, and the only one a
 * real host would have to write itself.
 *
 * It keeps the two promises {@link ProposalApplier} asks for: a ledger keyed by
 * `operationId` that outlives the call (so recovery can ask "did my write
 * land?"), and a call counter, which is how every replay assertion below proves
 * the work happened exactly once.
 */
class FakeApplier implements ProposalApplier {
  /** operationId → what landed under it. Survives a simulated crash. */
  readonly ledger = new Map<
    string,
    { operationId: string; proposalId: string; outcome: ApplyOutcome }
  >();
  /** Applies that actually reached the world. The replay guard's witness. */
  calls = 0;

  async apply(input: ApplyProposalInput): Promise<ApplyOutcome> {
    this.calls += 1;
    const outcome: ApplyOutcome = {
      status: "applied",
      appliedOps: 1,
      failedOps: [],
    };
    this.ledger.set(input.operationId, {
      operationId: input.operationId,
      proposalId: input.proposal.id,
      outcome,
    });
    return outcome;
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.ledger.get(operationId)?.outcome ?? null;
  }

  /** Seed the ledger for a crash that happened AFTER the write landed. */
  remember(operationId: string, proposalId: string): void {
    this.ledger.set(operationId, {
      operationId,
      proposalId,
      outcome: { status: "applied", appliedOps: 1, failedOps: [] },
    });
  }
}

// ---------------------------------------------------------------------------
// Provider doubles
// ---------------------------------------------------------------------------

/**
 * Two turns: ask for the write, then answer. Tool calls are announced ONLY via
 * `run.tool.requested` (the mock's default) — the streaming shape, where the
 * assistant record has to be persisted open and filled in as the calls arrive.
 */
function scriptedProvider(): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: TOOL_CALL_ID,
          name: NOTES_APPEND.name,
          argumentsJson: JSON.stringify({
            action_id: ACTION_ID,
            text: NOTE_TEXT,
          }),
        },
      ],
    },
    { steps: [{ kind: "text", content: FINAL_ANSWER }] },
  ]);
  return provider;
}

/**
 * Records what each provider round-trip was handed.
 *
 * The prompt the SECOND call receives is assembled from persisted records, not
 * from anything the first call kept in memory — so what it contains is the real
 * test of whether the turn worker persisted a replayable conversation.
 */
class RecordingProviderClient implements AiProviderClient {
  readonly id = "recording";
  readonly kind = "openai-compatible" as const;
  /** Messages per call, deep-enough copies to survive later mutation. */
  readonly calls: AiChatMessage[][] = [];

  constructor(private readonly inner: AiProviderClient) {}

  async capabilities(signal?: AbortSignal, model?: string) {
    return this.inner.capabilities(signal, model);
  }

  async listModels(signal?: AbortSignal) {
    return this.inner.listModels(signal);
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.calls.push(input.messages.map((message) => ({ ...message })));
    yield* this.inner.streamChat(input);
  }
}

// ---------------------------------------------------------------------------
// The wiring — this is the part a host copies
// ---------------------------------------------------------------------------

interface Slice {
  clock: TestClock;
  store: SqliteAssistantStore;
  taskRunner: SingleProcessTaskRunner;
  turnRunner: TurnRunner;
  policy: SessionWritePolicy;
  proposals: ProposalService;
  applier: FakeApplier;
  provider: RecordingProviderClient;
  /** How many times the write tool's `build` ran. */
  builds(): number;
}

function embedSlice(options: { provider?: AiProviderClient } = {}): Slice {
  // 1. Ambient ports. Injected, not imported, so every assertion below is about
  //    the fake clock rather than about how fast this machine is.
  const clock = createTestClock();
  const ids = createSequentialIds();

  // 2. One durable substrate behind every port.
  const store = new SqliteAssistantStore(":memory:", { clock, ids });

  // 3. The queue that will call the worker back.
  const taskRunner = new SingleProcessTaskRunner({
    store,
    clock,
    pollMs: 5,
    // Far past any test's real lifetime: renewal must not rescue or expire a
    // lease behind a test's back.
    heartbeatMs: 60_000,
  });

  // 4. The write pipeline. The policy decides whether a staged write applies
  //    itself; the applier is the only thing that touches the world.
  const policy = new SessionWritePolicy({
    mode: "auto_readonly_confirm_writes",
    clock,
  });
  const applier = new FakeApplier();
  const proposals = new ProposalService({
    store,
    applier,
    policy,
    clock,
    ids,
  });

  // 5. The host's one write tool. `build` is the ONLY host domain logic here —
  //    staging, dedup, the auto-apply gate and the model-facing projection all
  //    come from the framework.
  let builds = 0;
  const notesAppend = createProposalBuilderTool<NotesAppendInput>({
    definition: NOTES_APPEND,
    service: proposals,
    store,
    policy,
    ids,
    // The scope comes from the run, not from a constant in this file: the
    // framework threads `TaskRecord.scopeId` through `runChat` onto every tool
    // context, which is the only thing that knows what this turn writes to.
    scopeKeyOf: (ctx) => ctx.scopeId ?? SCOPE_KEY,
    build: async (_ctx, input) => {
      builds += 1;
      return {
        kind: PROPOSAL_KIND,
        risk: "medium",
        operations: [{ op: "append", text: input.text }],
        warnings: [],
        truncated: false,
        envelope: { summary: `Append "${input.text}"`, text: input.text },
      };
    },
  });

  // 6. Tools are contributed per run, not registered at boot.
  const contributor: ToolSetContributor = {
    contribute: async () => [notesAppend as unknown as AiTool],
  };

  // 7. The worker. `providerFactory` is handed the resolved provider config
  //    (key already injected); here every run gets the same scripted client.
  const provider = new RecordingProviderClient(
    options.provider ?? scriptedProvider(),
  );
  const turnRunner = new TurnRunner({
    store,
    taskRunner,
    providerFactory: () => provider,
    contributors: [contributor],
    clock,
    ids,
  });

  return {
    clock,
    store,
    taskRunner,
    turnRunner,
    policy,
    proposals,
    applier,
    provider,
    builds: () => builds,
  };
}

/** Provider + settings + chat rows a host would have written before any turn. */
async function seedHostState(slice: Slice): Promise<void> {
  await slice.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await slice.store.settings.updateSettings({ defaultProviderId: "p1" });
  await slice.store.conversations.createChat({ id: CHAT_ID });
}

/**
 * Run the whole slice once: start the worker, submit, wait for the run to reach
 * a terminal state, then stop the worker.
 *
 * The worker is stopped INSIDE the helper so every assertion afterwards reads a
 * settled store — nothing is racing the dispatch loop while a test looks at it.
 */
async function submitAndSettle(
  slice: Slice,
  content: string,
  options: { onceRunning?: (runId: string) => Promise<void> } = {},
): Promise<SubmitMessageResult> {
  // The queue is handed the DISPATCHER, not the turn worker: a chat turn is one
  // registered kind, and the slice wires it the way a host with several kinds
  // would.
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(slice.turnRunner));
  const handle = await slice.taskRunner.startWorker(
    createDispatchingWorker(registry, {
      store: slice.store,
      clock: slice.clock,
    }),
    { concurrency: 1, ownerId: "owner-e2e" },
  );
  try {
    const submitted = await slice.turnRunner.submitMessage({
      chatId: CHAT_ID,
      content,
    });
    await options.onceRunning?.(submitted.runId);
    await waitFor(
      async () => isTerminal(await runStatus(slice, submitted.runId)),
      "the submitted run to reach a terminal state",
    );
    return submitted;
  } finally {
    await handle.stop();
  }
}

async function runStatus(slice: Slice, runId: string): Promise<string> {
  return (await slice.store.tasks.getTask(runId))?.status ?? "missing";
}

/** The turn's event log, narrowed to the chat-turn vocabulary it holds. */
async function eventsOf(slice: Slice, taskId: string): Promise<AiRunEvent[]> {
  return (await slice.store.tasks.listEvents(taskId)) as AiRunEvent[];
}

function isTerminal(status: string): boolean {
  return (
    status === "completed" || status === "failed" || status === "cancelled"
  );
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

const TERMINAL_EVENT_TYPES = new Set<AiRunEvent["type"]>([
  "run.completed",
  "run.failed",
  "run.cancelled",
]);

/**
 * The property a consumer of the event log depends on and nobody owns: seq is
 * gapless from 0, every event is self-describing (`contractVersion` + a unique
 * `eventId`), and the run ends exactly once, at the end.
 */
function expectOneUnbrokenStream(
  events: AiRunEvent[],
  terminalType: AiRunEvent["type"],
): void {
  expect(events.length).toBeGreaterThan(0);
  expect(events.map((event) => event.seq)).toEqual(
    events.map((_, index) => index),
  );
  for (const event of events) {
    expect(event.contractVersion).toBe(CONTRACT_VERSION);
    expect(typeof event.eventId).toBe("string");
    expect(event.eventId.length).toBeGreaterThan(0);
  }
  expect(new Set(events.map((event) => event.eventId)).size).toBe(
    events.length,
  );

  const terminals = events.filter((event) =>
    TERMINAL_EVENT_TYPES.has(event.type),
  );
  expect(terminals.map((event) => event.type)).toEqual([terminalType]);
  expect(terminals[0]?.eventId).toBe(events[events.length - 1]?.eventId);
}

/** The slim envelope a `role: "tool"` record carries, with its `modelData`. */
function envelopeOf(
  record: MessageRecord,
): AiToolEnvelope & { data: WriteToolModelData } {
  return JSON.parse(record.content) as AiToolEnvelope & {
    data: WriteToolModelData;
  };
}

function toolRecord(messages: MessageRecord[]): MessageRecord {
  const record = messages.find((message) => message.role === "tool");
  expect(record).toBeDefined();
  return record!;
}

/** The replay-only assistant turn carrying the tool calls. */
function internalAssistantRecord(messages: MessageRecord[]): MessageRecord {
  const record = messages.find(
    (message) =>
      message.role === "assistant" && message.metadata["internal"] === true,
  );
  expect(record).toBeDefined();
  return record!;
}

async function onlyProposal(slice: Slice): Promise<ProposalRecord> {
  const staged = await slice.store.proposals.listByChat(CHAT_ID);
  expect(staged).toHaveLength(1);
  return staged[0]!;
}

// ---------------------------------------------------------------------------
// Scenario A — the default posture: a write waits for a human
// ---------------------------------------------------------------------------

describe("e2e vertical slice (A) — a write stages, waits, then applies once", () => {
  it("carries a tool-calling turn end to end and leaves the write pending", async () => {
    const slice = embedSlice();
    await seedHostState(slice);
    try {
      const submitted = await submitAndSettle(slice, "add a note");

      // (1) The run landed where a completed turn should, on one attempt.
      const run = await slice.store.tasks.getTask(submitted.runId);
      expect(run?.status).toBe("completed");
      expect(run?.attemptCount).toBe(1);
      expect(run?.finishedAt).toBeDefined();

      // (2) The placeholder written at submit time is where the answer landed.
      const messages = await slice.store.conversations.listMessages(CHAT_ID);
      const placeholder = messages.find(
        (message) => message.id === submitted.assistantMessageId,
      );
      expect(placeholder?.content).toBe(FINAL_ANSWER);
      expect(placeholder?.metadata["placeholder"]).toBe(false);
      expect(messages[0]?.role).toBe("user");
      expect(messages[0]?.id).toBe(submitted.userMessageId);

      // (3) Replay state: the internal assistant turn carries the tool call it
      //     made, and the tool record carries the SLIM envelope — the model's
      //     view of the write, not the host's payload.
      const internal = internalAssistantRecord(messages);
      expect(internal.toolCalls).toEqual([
        {
          id: TOOL_CALL_ID,
          name: NOTES_APPEND.name,
          argumentsJson: JSON.stringify({
            action_id: ACTION_ID,
            text: NOTE_TEXT,
          }),
        },
      ]);
      const tool = toolRecord(messages);
      expect(tool.toolCallId).toBe(TOOL_CALL_ID);
      expect(tool.metadata["toolName"]).toBe(NOTES_APPEND.name);
      expect(tool.modelResultJson).toBe(tool.content);
      const envelope = envelopeOf(tool);
      expect(envelope.ok).toBe(true);
      expect(envelope.status).toBe("ok");
      expect(envelope.data).toEqual({
        status: "pending",
        appliedCount: 0,
        skipped: [],
      });
      // The model sees four statuses and a count — nothing else. The host's
      // staged payload is NOT in replayed history.
      expect(Object.keys(envelope.data).sort()).toEqual([
        "appliedCount",
        "skipped",
        "status",
      ]);
      // Neither the scope it was staged against nor the payload it staged
      // reaches the model's replayed history.
      expect(tool.content).not.toContain(CHAT_ID);
      expect(tool.content).not.toContain(NOTE_TEXT);

      // (4) Exactly one proposal, staged and waiting — against the RUN's scope,
      //     which reached the write tool as `ctx.scopeId` (a turn is scoped on
      //     its chat; a host writing a shared document would scope on that).
      const proposal = await onlyProposal(slice);
      expect(proposal.scopeKey).toBe(CHAT_ID);
      expect(proposal.status).toBe("pending");
      expect(proposal.actionId).toBe(ACTION_ID);
      expect(proposal.toolName).toBe(NOTES_APPEND.name);
      expect(proposal.kind).toBe(PROPOSAL_KIND);
      expect(proposal.risk).toBe("medium");
      expect(proposal.runId).toBe(submitted.runId);
      expect(proposal.envelope).toEqual({
        summary: `Append "${NOTE_TEXT}"`,
        text: NOTE_TEXT,
      });
      expect(proposal.operations).toEqual([{ op: "append", text: NOTE_TEXT }]);
      expect(proposal.decision).toBeUndefined();
      // Nothing was written: the policy never said it could be.
      expect(slice.applier.calls).toBe(0);

      // (5) One unbroken, gap-detectable event stream across BOTH provider
      //     round-trips, ending exactly once.
      const events = await eventsOf(slice, submitted.runId);
      expectOneUnbrokenStream(events, "run.completed");

      // (6) The tool's whole visible life is in the log.
      const toolEvents = events.filter((event) =>
        event.type.startsWith("run.tool."),
      );
      expect(toolEvents.map((event) => event.type)).toEqual([
        "run.tool.requested",
        "run.tool.running",
        "run.tool.succeeded",
      ]);
      for (const event of toolEvents) {
        const data = event.data as { toolName: string; toolCallId: string };
        expect(data.toolName).toBe(NOTES_APPEND.name);
        expect(data.toolCallId).toBe(TOOL_CALL_ID);
      }
      expect(slice.builds()).toBe(1);

      // The full/slim split: the UI-facing projection (proposal id, risk,
      // operation count, the whole envelope) rides on the event, while the
      // message the model replays carries only the slim envelope above.
      const succeeded = toolEvents.at(-1)!;
      const full = JSON.parse(
        (succeeded.data as { resultJson: string }).resultJson,
      ) as Record<string, unknown>;
      expect(full["proposalId"]).toBe(proposal.id);
      expect(full["operationCount"]).toBe(1);
      expect(full["risk"]).toBe("medium");
      expect(full["envelope"]).toEqual(proposal.envelope);

      // (7) The SECOND provider round-trip is assembled from persisted records,
      //     and it is provider-legal: the assistant turn that asked, then the
      //     tool result that answered it, joined by tool_call_id. The empty
      //     placeholder is not replayed.
      expect(slice.provider.calls).toHaveLength(2);
      const replay = slice.provider.calls[1]!;
      expect(replay.map((message) => message.role)).toEqual([
        "user",
        "assistant",
        "tool",
      ]);
      expect(replay[0]?.content).toBe("add a note");
      expect(replay[1]?.toolCalls?.[0]?.id).toBe(TOOL_CALL_ID);
      expect(replay[2]?.toolCallId).toBe(TOOL_CALL_ID);
      expect(replay[2]?.name).toBe(NOTES_APPEND.name);
      expect(replay[2]?.content).toBe(tool.content);
    } finally {
      slice.store.close();
    }
  });

  it("applies once on approval, and replays the outcome instead of applying twice", async () => {
    const slice = embedSlice();
    await seedHostState(slice);
    try {
      await submitAndSettle(slice, "add a note");
      const staged = await onlyProposal(slice);

      // The human decision, recorded separately from the work it authorises.
      const approved = await slice.proposals.approve({
        proposalId: staged.id,
        actor: "user",
        decidedBy: "operator@example.test",
      });
      expect(approved.status).toBe("approved");
      expect(approved.decision?.actor).toBe("user");
      expect(approved.decision?.policyId).toBeUndefined();

      const outcome = await slice.proposals.apply({
        proposalId: staged.id,
        operationId: "op-e2e-1",
      });
      expect(outcome).toEqual({
        status: "applied",
        appliedOps: 1,
        failedOps: [],
      });
      expect(slice.applier.calls).toBe(1);
      const applied = await slice.store.proposals.get(staged.id);
      expect(applied?.status).toBe("applied");
      expect(applied?.operationId).toBe("op-e2e-1");
      expect(applied?.appliedAt).toBeDefined();

      // REPLAY: the same operation id, redelivered. The recorded outcome comes
      // back verbatim and the world is not touched a second time.
      const replayed = await slice.proposals.apply({
        proposalId: staged.id,
        operationId: "op-e2e-1",
      });
      expect(replayed).toEqual(outcome);
      expect(slice.applier.calls).toBe(1);
      expect((await slice.store.proposals.get(staged.id))?.status).toBe(
        "applied",
      );
    } finally {
      slice.store.close();
    }
  });

  it("reconciles a crash mid-apply from the applier's ledger, not from a guess", async () => {
    const slice = embedSlice();
    await seedHostState(slice);
    try {
      // Two proposals staged straight through the service — the crash window is
      // between "the applier ran" and "we wrote that down", which no tool call
      // can reach on purpose.
      const landed = await stageApproveAndClaim(
        slice,
        "append_note-2_scope-A",
        "op-crash-landed",
      );
      const lost = await stageApproveAndClaim(
        slice,
        "append_note-3_scope-A",
        "op-crash-lost",
      );

      // The applier DID run for the first one before the process died…
      slice.applier.remember("op-crash-landed", landed.id);
      // …and for the second one nobody can prove anything.

      const report = await slice.proposals.reconcileInterrupted();
      expect(report).toEqual({ reconciled: 2, applied: 1, failed: 1 });

      const finalized = await slice.store.proposals.get(landed.id);
      expect(finalized?.status).toBe("applied");
      // The applier's own record is now persisted on our side too.
      expect(await slice.store.proposals.getOutcome("op-crash-landed")).toEqual(
        {
          status: "applied",
          appliedOps: 1,
          failedOps: [],
        },
      );

      const abandoned = await slice.store.proposals.get(lost.id);
      expect(abandoned?.status).toBe("failed");
      expect(abandoned?.reason).toBe("interrupted");

      // Reconciliation asks the applier; it never re-runs it.
      expect(slice.applier.calls).toBe(0);
    } finally {
      slice.store.close();
    }
  });
});

/**
 * Stage → approve → claim, leaving the proposal in `applying` with an operation
 * id: exactly the durable state a process that dies mid-apply leaves behind.
 */
async function stageApproveAndClaim(
  slice: Slice,
  actionId: string,
  operationId: string,
): Promise<ProposalRecord> {
  const proposal = await slice.proposals.stage({
    chatId: CHAT_ID,
    scopeKey: SCOPE_KEY,
    actionId,
    toolName: NOTES_APPEND.name,
    kind: PROPOSAL_KIND,
    risk: "medium",
    operations: [{ op: "append", text: actionId }],
  });
  await slice.proposals.approve({ proposalId: proposal.id, actor: "user" });
  return slice.store.proposals.transition(
    proposal.id,
    ["approved"],
    "applying",
    { operationId },
  );
}

// ---------------------------------------------------------------------------
// Scenario B — a standing allowance: the same turn applies itself
// ---------------------------------------------------------------------------

describe("e2e vertical slice (B) — a policy-approved write applies in the turn", () => {
  it("auto-applies under an allowance, records a POLICY decision, and tells the model it landed", async () => {
    const slice = embedSlice();
    await seedHostState(slice);
    try {
      // The user's standing "yes" for this chat + tool + kind, up to medium risk.
      const allowance = slice.policy.allow({
        chatId: CHAT_ID,
        toolName: NOTES_APPEND.name,
        proposalKind: PROPOSAL_KIND,
        maxRisk: "medium",
      });
      expect(allowance.key).toBe(
        `${CHAT_ID}:${NOTES_APPEND.name}:${PROPOSAL_KIND}`,
      );

      const submitted = await submitAndSettle(slice, "add a note");
      expect((await slice.store.tasks.getTask(submitted.runId))?.status).toBe(
        "completed",
      );

      // The write landed inside the turn, with nobody calling `approve`.
      const proposal = await onlyProposal(slice);
      expect(proposal.status).toBe("applied");
      expect(proposal.decision?.actor).toBe("policy");
      expect(proposal.decision?.policyId).toBe("session-write-policy");
      expect(proposal.operationId).toBeDefined();
      expect(proposal.appliedAt).toBeDefined();
      expect(slice.applier.calls).toBe(1);
      expect(
        await slice.store.proposals.getOutcome(proposal.operationId!),
      ).toEqual({ status: "applied", appliedOps: 1, failedOps: [] });
      // The applier saw the proposal it was told to apply, under that id.
      expect(slice.applier.ledger.get(proposal.operationId!)?.proposalId).toBe(
        proposal.id,
      );

      // And what the model was told says "done", not "waiting".
      const messages = await slice.store.conversations.listMessages(CHAT_ID);
      const envelope = envelopeOf(toolRecord(messages));
      expect(envelope.ok).toBe(true);
      expect(envelope.status).toBe("ok");
      expect(envelope.data).toEqual({
        status: "ok",
        appliedCount: 1,
        skipped: [],
      });

      const placeholder = messages.find(
        (message) => message.id === submitted.assistantMessageId,
      );
      expect(placeholder?.content).toBe(FINAL_ANSWER);
      expect(placeholder?.metadata["placeholder"]).toBe(false);
      expectOneUnbrokenStream(
        await eventsOf(slice, submitted.runId),
        "run.completed",
      );
    } finally {
      slice.store.close();
    }
  });
});

// ---------------------------------------------------------------------------
// Scenario C — cancellation mid-stream
// ---------------------------------------------------------------------------

describe("e2e vertical slice (C) — a cancel mid-stream still lands cleanly", () => {
  it("cancels a run parked in the provider, finalizing the placeholder and the stream", async () => {
    const provider = new HangingProviderClient();
    const slice = embedSlice({ provider });
    await seedHostState(slice);
    try {
      const submitted = await submitAndSettle(slice, "add a note", {
        onceRunning: async (runId) => {
          // Deterministic: cancel only once the provider is demonstrably parked
          // mid-stream, with a delta already persisted.
          await waitFor(
            () => provider.blocking,
            "the provider to park mid-stream",
          );
          await slice.turnRunner.cancel(runId);
        },
      });

      const run = await slice.store.tasks.getTask(submitted.runId);
      expect(run?.status).toBe("cancelled");
      expect(run?.finishedAt).toBeDefined();

      // The placeholder is finalized — with the partial answer, not erased.
      const messages = await slice.store.conversations.listMessages(CHAT_ID);
      const placeholder = messages.find(
        (message) => message.id === submitted.assistantMessageId,
      );
      expect(placeholder?.content).toBe("Thinking");
      expect(placeholder?.metadata["placeholder"]).toBe(false);

      // One attempt, ended cancelled; no retry of a run the user stopped.
      const events = await eventsOf(slice, submitted.runId);
      expectOneUnbrokenStream(events, "run.cancelled");
      expect(events.map((event) => event.type)).toEqual([
        "run.started",
        "run.message.delta",
        "run.cancelled",
      ]);
      const attemptIds = new Set(events.map((event) => event.attemptId));
      expect(attemptIds.size).toBe(1);

      // Nothing was staged and nothing was written: the tool never ran.
      expect(await slice.store.proposals.listByChat(CHAT_ID)).toHaveLength(0);
      expect(slice.applier.calls).toBe(0);
    } finally {
      slice.store.close();
    }
  });
});
