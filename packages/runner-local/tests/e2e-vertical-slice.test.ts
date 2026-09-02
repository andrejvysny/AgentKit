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
 * - "apply" is safe to say twice, and safe to be interrupted in the middle of;
 * - a worker that dies mid-turn is CONTINUED by the next one — same chain, same
 *   event stream — and the attempt it left behind cannot land its answer when
 *   it finally wakes up.
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
  type WorkerHandle,
  type WriteToolModelData,
} from "@agentkit/host";
import { HangingProviderClient, MockProviderClient } from "@agentkit/testing";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { SingleProcessTaskRunner } from "../src/index.js";
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
    chatId: () => next("chat"),
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
  /**
   * The id generator the store and the worker share. Exposed for scenario D,
   * which stands a SECOND worker up over this same store: two counters would
   * mint one message id twice.
   */
  ids: IdGenerator;
  /** The turn's tool set, so that second worker contributes the same one. */
  contributor: ToolSetContributor;
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
    namespace: "notes",
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
    ids,
    contributor,
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
 * Start a worker over one runner + turn-runner pair.
 *
 * The queue is handed the DISPATCHER, not the turn worker: a chat turn is one
 * registered kind, and the slice wires it the way a host with several kinds
 * would. The pair is an ARGUMENT rather than read off the slice because
 * scenario D stands a second worker up over the same store.
 */
async function startTurnWorker(
  slice: Slice,
  runners: { taskRunner: SingleProcessTaskRunner; turnRunner: TurnRunner },
  ownerId: string,
): Promise<WorkerHandle> {
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(runners.turnRunner));
  return runners.taskRunner.startWorker(
    createDispatchingWorker(registry, {
      store: slice.store,
      clock: slice.clock,
    }),
    { concurrency: 1, ownerId },
  );
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
  const handle = await startTurnWorker(slice, slice, "owner-e2e");
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

/**
 * The slim envelope a `role: "tool"` record carries, with its `modelData`.
 *
 * `MessageRecord.content` is `string | AiContentPart[]`, but a tool result is a
 * serialized envelope by construction — so the string-ness is asserted here
 * rather than cast away, and a tool record that somehow arrived as parts fails
 * loudly instead of parsing as `"[object Object]"`.
 */
function envelopeOf(
  record: MessageRecord,
): AiToolEnvelope & { data: WriteToolModelData } {
  expect(typeof record.content).toBe("string");
  return JSON.parse(record.content as string) as AiToolEnvelope & {
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
      expect(tool.modelResultJson).toBe(tool.content as string);
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
      // C14: the key is the JSON tuple, not a `:`-joined string — every member
      // is caller (or model) data, and a separator that can appear inside one
      // makes two different grants collide. `null` is the scope: this grant
      // was given for the chat + tool + kind and covers every scope.
      expect(allowance.key).toBe(
        JSON.stringify([CHAT_ID, NOTES_APPEND.name, PROPOSAL_KIND, null]),
      );
      expect(allowance.scopeKey).toBeUndefined();

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

// ---------------------------------------------------------------------------
// Scenario D — a worker dies mid-turn, and comes back to find it lost
// ---------------------------------------------------------------------------

/**
 * `SqliteAssistantStore`'s own `DEFAULT_LEASE_TTL_MS`: how long a dead owner's
 * claim on a task outlives the process that made it.
 */
const LEASE_TTL_MS = 30_000;
/** The tool call the RECOVERED attempt makes — a fresh action, so nothing dedups. */
const RECOVERY_TOOL_CALL_ID = "call-notes-2";
const RECOVERY_ACTION_ID = "append_note-2_scope-A";
const RECOVERY_NOTE_TEXT = "again";
/** What the crashed attempt was about to say when its process stopped existing. */
const ZOMBIE_ANSWER = "ZOMBIE";
/** What the attempt that actually owns the turn says. */
const RECOVERED_ANSWER = "Done.";

/**
 * A place for a provider call to die, and to wake up in much later.
 *
 * Deliberately NOT signal-aware, which is the whole point: `HangingProviderClient`
 * parks until it is ABORTED, and a crash is precisely the case where nobody is
 * left to abort anything. `whenParked` is the deterministic handle — the call is
 * demonstrably inside the provider with everything it streamed so far already
 * durable — and `release` is the zombie waking up.
 */
class ProviderGate {
  private announceParked!: () => void;
  private open!: () => void;
  private readonly parked: Promise<void>;
  private readonly opened: Promise<void>;

  constructor() {
    this.parked = new Promise<void>((resolve) => {
      this.announceParked = resolve;
    });
    this.opened = new Promise<void>((resolve) => {
      this.open = resolve;
    });
  }

  /** Resolves once a call has parked here. Await instead of polling. */
  whenParked(): Promise<void> {
    return this.parked;
  }

  /** Called from inside the stream: announce, then wait to be let go. */
  async park(): Promise<void> {
    this.announceParked();
    await this.opened;
  }

  /** Idempotent, so a `finally` can unblock a test that failed mid-park. */
  release(): void {
    this.open();
  }
}

/**
 * The provider a dead worker leaves behind.
 *
 * The turns are a {@link MockProviderClient}'s, so this double invents no event
 * shapes of its own. What it adds is a PARK: on a gated call the stream stops
 * immediately before `run.message.completed` — `run.started` and every delta
 * already persisted, the answer itself not yet committed — and waits on a
 * promise `input.signal` cannot reach.
 */
class ParkingProviderClient implements AiProviderClient {
  readonly id = "parking";
  readonly kind = "openai-compatible" as const;
  /** Provider round-trips so far. */
  callCount = 0;

  constructor(
    private readonly inner: MockProviderClient,
    /** 1-based call number → the gate that call parks on. */
    private readonly gates: Map<number, ProviderGate>,
  ) {}

  async capabilities() {
    return this.inner.capabilities();
  }

  async listModels() {
    return this.inner.listModels();
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.callCount += 1;
    const gate = this.gates.get(this.callCount);
    for await (const event of this.inner.streamChat(input)) {
      if (gate !== undefined && event.type === "run.message.completed") {
        await gate.park();
      }
      yield event;
    }
  }
}

/**
 * Four round-trips: the crashed attempt asks for a write and then dies
 * mid-answer; the recovered attempt asks for its own write and then answers.
 */
function crashingProvider(
  gates: Map<number, ProviderGate>,
): ParkingProviderClient {
  const inner = new MockProviderClient();
  inner.setScript([
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
    { steps: [{ kind: "text", content: ZOMBIE_ANSWER }] },
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: RECOVERY_TOOL_CALL_ID,
          name: NOTES_APPEND.name,
          argumentsJson: JSON.stringify({
            action_id: RECOVERY_ACTION_ID,
            text: RECOVERY_NOTE_TEXT,
          }),
        },
      ],
    },
    { steps: [{ kind: "text", content: RECOVERED_ANSWER }] },
  ]);
  return new ParkingProviderClient(inner, gates);
}

/**
 * The second process: a runner and a turn worker of its own, over the SAME
 * durable store. The tail of {@link embedSlice} from step 3 on, which is
 * exactly what a host that restarted after a crash would rebuild.
 *
 * It shares the slice's clock (one fake "now" for both), its id generator (two
 * counters would mint one message id twice) and its provider double (the model
 * does not restart when the host does).
 */
function embedSecondRunner(slice: Slice): {
  taskRunner: SingleProcessTaskRunner;
  turnRunner: TurnRunner;
} {
  const taskRunner = new SingleProcessTaskRunner({
    store: slice.store,
    clock: slice.clock,
    pollMs: 5,
    heartbeatMs: 60_000,
  });
  const turnRunner = new TurnRunner({
    store: slice.store,
    taskRunner,
    providerFactory: () => slice.provider,
    contributors: [slice.contributor],
    clock: slice.clock,
    ids: slice.ids,
  });
  return { taskRunner, turnRunner };
}

describe("e2e vertical slice (D) — a crashed attempt is continued, and its zombie cannot land", () => {
  it("continues the crashed turn's chain on a second attempt, and refuses the zombie's answer", async () => {
    const zombieGate = new ProviderGate();
    const provider = crashingProvider(new Map([[2, zombieGate]]));
    const slice = embedSlice({ provider });
    await seedHostState(slice);
    const second = embedSecondRunner(slice);
    let workerA: WorkerHandle | null = null;
    let workerB: WorkerHandle | null = null;
    try {
      workerA = await startTurnWorker(slice, slice, "owner-A");
      const submitted = await slice.turnRunner.submitMessage({
        chatId: CHAT_ID,
        content: "add a note",
      });

      // (1) THE CRASH. The worker gets as far as its second round-trip and
      //     stops existing: the tool it asked for has already run, its records
      //     are already durable, and the answer is half-streamed into a
      //     placeholder nothing is coming back to finish. Nobody aborts it —
      //     that is what makes it a crash rather than a cancel.
      await zombieGate.whenParked();
      expect(provider.callCount).toBe(2);
      const crashed = await slice.store.conversations.listMessages(CHAT_ID);
      expect(crashed.map((message) => message.role)).toEqual([
        "user",
        "assistant",
        "assistant",
        "tool",
      ]);
      const stranded = crashed.find(
        (message) => message.id === submitted.assistantMessageId,
      );
      expect(stranded?.metadata["placeholder"]).toBe(true);
      expect(internalAssistantRecord(crashed).toolCalls?.[0]?.id).toBe(
        TOOL_CALL_ID,
      );
      expect(toolRecord(crashed).toolCallId).toBe(TOOL_CALL_ID);
      const crashedTask = await slice.store.tasks.getTask(submitted.runId);
      expect(crashedTask?.status).toBe("running");
      expect(crashedTask?.attemptCount).toBe(1);

      // (2) THE RECOVERY. The dead owner's lease is the only evidence a process
      //     stopped, so nothing moves until it expires. A SECOND process then
      //     finds it and hands the task a fresh attempt.
      slice.clock.advance(LEASE_TTL_MS + 1);
      workerB = await startTurnWorker(slice, second, "owner-B");
      await second.taskRunner.recover();
      await waitFor(
        async () => isTerminal(await runStatus(slice, submitted.runId)),
        "the recovered attempt to reach a terminal state",
        5_000,
      );

      // (3) The task landed on the SECOND attempt of the same run — same id,
      //     same log, one more attempt. (`poisonCount` is asserted elsewhere.)
      const run = await slice.store.tasks.getTask(submitted.runId);
      expect(run?.status).toBe("completed");
      expect(run?.attemptCount).toBe(2);
      expect(run?.finishedAt).toBeDefined();
      expect(run?.error).toBeUndefined();

      // (4) ONE unbroken event stream across the attempt boundary: gapless seq
      //     from 0, unique ids, and exactly one terminal — at the end, from the
      //     attempt that actually finished.
      const events = await eventsOf(slice, submitted.runId);
      expectOneUnbrokenStream(events, "run.completed");
      const attemptIds = [...new Set(events.map((event) => event.attemptId))];
      expect(attemptIds).toHaveLength(2);
      const boundary = events.findIndex(
        (event) => event.attemptId === attemptIds[1],
      );
      expect(
        events
          .slice(0, boundary)
          .every((event) => event.attemptId === attemptIds[0]),
      ).toBe(true);
      expect(
        events
          .slice(boundary)
          .every((event) => event.attemptId === attemptIds[1]),
      ).toBe(true);
      // Attempt 2 opens with its own `run.started`, after everything attempt 1
      // had managed to write — which ends on the delta it was streaming when
      // the process died.
      expect(events[boundary]?.type).toBe("run.started");
      expect(events[boundary - 1]?.type).toBe("run.message.delta");

      // (5) THE CHAIN. Attempt 2's records continue attempt 1's chain and stay
      //     ON THE ACTIVE PATH — the path every later turn replays.
      const messages = await slice.store.conversations.listMessages(CHAT_ID);
      expect(messages.map((message) => message.role)).toEqual([
        "user",
        "assistant",
        "assistant",
        "tool",
        "assistant",
        "tool",
      ]);
      const placeholder = messages[1];
      const firstInternal = messages[2];
      const firstTool = messages[3];
      const secondInternal = messages[4];
      const secondTool = messages[5];
      expect(placeholder?.id).toBe(submitted.assistantMessageId);
      expect(firstInternal?.toolCalls?.[0]?.id).toBe(TOOL_CALL_ID);
      expect(secondInternal?.toolCalls?.[0]?.id).toBe(RECOVERY_TOOL_CALL_ID);
      expect(secondTool?.toolCallId).toBe(RECOVERY_TOOL_CALL_ID);
      // THE `lastMessageOfRun` RULE. Attempt 2 hangs its first record off the
      // record attempt 1 wrote LAST, not off the placeholder — which by then
      // already has an active child, so a sibling there would land
      // `active: false` and take attempt 2's whole turn off the path.
      expect(firstInternal?.parentMessageId).toBe(placeholder?.id);
      expect(secondInternal?.parentMessageId).toBe(firstTool?.id);
      expect(secondTool?.parentMessageId).toBe(secondInternal?.id);
      for (const message of messages) expect(message.active).toBe(true);

      // (6) The recovered attempt's FIRST round-trip is assembled from what a
      //     process it never met persisted: the tool call, and the result that
      //     answered it.
      expect(slice.provider.calls).toHaveLength(4);
      const replay = slice.provider.calls[2]!;
      expect(replay.map((message) => message.role)).toEqual([
        "user",
        "assistant",
        "tool",
      ]);
      expect(replay[1]?.toolCalls?.[0]?.id).toBe(TOOL_CALL_ID);
      expect(replay[2]?.toolCallId).toBe(TOOL_CALL_ID);
      expect(replay[2]?.content).toBe(firstTool?.content);

      // (7) The placeholder is finalized with the answer of the attempt that
      //     owned the task.
      expect(placeholder?.content).toBe(RECOVERED_ANSWER);
      expect(placeholder?.metadata["placeholder"]).toBe(false);

      // (8) THE ZOMBIE WAKES, long after the turn it belonged to was answered
      //     by somebody else, and tries to commit what it was holding.
      const finishedAt = run?.finishedAt;
      zombieGate.release();
      // `stop()` waits its executions out, so this IS the zombie finishing:
      // nothing below is racing a write that is still on its way.
      await workerA.stop();
      workerA = null;
      expect(provider.callCount).toBe(4);

      // Every durable write an attempt makes is fenced on the lease it no
      // longer holds, and `appendEvents` is fenced FIRST — before anything it
      // would have projected into the conversation. So the zombie lands
      // nothing: not a terminal, not an event, not a record, and above all not
      // its answer.
      const settled = await slice.store.tasks.getTask(submitted.runId);
      expect(settled?.status).toBe("completed");
      expect(settled?.finishedAt).toBe(finishedAt!);
      expect(settled?.attemptCount).toBe(2);
      expect(settled?.error).toBeUndefined();
      expect(await eventsOf(slice, submitted.runId)).toHaveLength(
        events.length,
      );
      const after = await slice.store.conversations.listMessages(CHAT_ID);
      expect(after).toHaveLength(messages.length);
      expect(
        after.find((message) => message.id === submitted.assistantMessageId)
          ?.content,
      ).toBe(RECOVERED_ANSWER);
      // Not merely off the active path: the zombie appended no sibling either.
      expect(
        await slice.store.conversations.listSiblings(secondInternal!.id),
      ).toHaveLength(1);
    } finally {
      // Before the stops: a gate nobody released would park `stop()` forever
      // on the execution it is waiting out.
      zombieGate.release();
      await workerA?.stop();
      await workerB?.stop();
      slice.store.close();
    }
  });

  it("refuses the zombie while the attempt that replaced it is still mid-stream", async () => {
    const zombieGate = new ProviderGate();
    const recoveredGate = new ProviderGate();
    const provider = crashingProvider(
      new Map([
        [2, zombieGate],
        [3, recoveredGate],
      ]),
    );
    const slice = embedSlice({ provider });
    await seedHostState(slice);
    const second = embedSecondRunner(slice);
    let workerA: WorkerHandle | null = null;
    let workerB: WorkerHandle | null = null;
    try {
      workerA = await startTurnWorker(slice, slice, "owner-A");
      const submitted = await slice.turnRunner.submitMessage({
        chatId: CHAT_ID,
        content: "add a note",
      });
      await zombieGate.whenParked();

      slice.clock.advance(LEASE_TTL_MS + 1);
      workerB = await startTurnWorker(slice, second, "owner-B");
      await second.taskRunner.recover();
      // The replacement attempt is now parked in a provider call of its own —
      // so the task is `running`, which is the state a terminal write is
      // allowed to transition FROM. The lease is the only thing between the
      // zombie and the live attempt's verdict.
      await recoveredGate.whenParked();

      zombieGate.release();
      await workerA.stop();
      workerA = null;

      const contested = await slice.store.tasks.getTask(submitted.runId);
      expect(contested?.status).toBe("running");
      expect(contested?.finishedAt).toBeUndefined();
      expect(contested?.error).toBeUndefined();
      expect(contested?.attemptCount).toBe(2);
      const stillOpen = await slice.store.conversations.listMessages(CHAT_ID);
      expect(
        stillOpen.find((message) => message.id === submitted.assistantMessageId)
          ?.metadata["placeholder"],
      ).toBe(true);

      // And the attempt that does own the task still finishes, undisturbed.
      recoveredGate.release();
      await waitFor(
        async () => isTerminal(await runStatus(slice, submitted.runId)),
        "the recovered attempt to reach a terminal state",
        5_000,
      );
      const run = await slice.store.tasks.getTask(submitted.runId);
      expect(run?.status).toBe("completed");
      expect(run?.attemptCount).toBe(2);
      const placeholder = (
        await slice.store.conversations.listMessages(CHAT_ID)
      ).find((message) => message.id === submitted.assistantMessageId);
      expect(placeholder?.content).toBe(RECOVERED_ANSWER);
      expect(placeholder?.metadata["placeholder"]).toBe(false);
      expectOneUnbrokenStream(
        await eventsOf(slice, submitted.runId),
        "run.completed",
      );
    } finally {
      zombieGate.release();
      recoveredGate.release();
      await workerA?.stop();
      await workerB?.stop();
      slice.store.close();
    }
  });
});
