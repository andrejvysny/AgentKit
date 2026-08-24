/**
 * Minimal in-memory implementations of the host ports, for testing the services
 * in this package.
 *
 * They are deliberately small — Maps and arrays, `transaction` runs the callback
 * — but NOT lenient: the invariants the services depend on (transition legality,
 * `(scopeKey, actionId)` uniqueness, outcome idempotency, seq monotonicity,
 * lease ownership) are enforced here. A fake that accepted anything would let a
 * service bug pass every test and fail against the first real adapter.
 *
 * The reference adapters (in-memory + sqlite, with a shared conformance suite)
 * arrive in a later wave; these live in the test folder and ship to nobody.
 */
import type { AiToolCall, TaskEventEnvelope } from "@agentkit/contracts";
import {
  ACTION_ID_RELEASING_STATUSES,
  DuplicateActionIdError,
  DuplicateTaskError,
  InvalidProposalTransitionError,
  InvalidTaskTransitionError,
  LeaseLostError,
  RecordNotFoundError,
  SeqConflictError,
  assertProposalTransition,
  assertTaskTransition,
  type ApplyOutcome,
  type ApplyProposalInput,
  type AppendEventsOptions,
  type AppendMessageInput,
  type AssistantSettings,
  type AssistantStore,
  type AttemptRecord,
  type AcquireLeaseInput,
  type ChatRecord,
  type ClaimNextInput,
  type ClaimedTask,
  type Clock,
  type ConversationStore,
  type CreateAttemptInput,
  type CreateChatInput,
  type CreateProposalInput,
  type CreateTaskInput,
  type EndAttemptInput,
  type EnqueueInput,
  type IdGenerator,
  type Lease,
  type ListEventsOptions,
  type ListMessagesOptions,
  type ListProposalsOptions,
  type MessageRecord,
  type OutboxAppendInput,
  type OutboxClaimInput,
  type OutboxRecord,
  type OutboxStore,
  type ProposalApplier,
  type ProposalPatch,
  type ProposalRecord,
  type ProposalStatus,
  type ProposalStore,
  type ProviderStore,
  type SettingsStore,
  type StartWorkerOptions,
  type TaskPatch,
  type TaskRecord,
  type TaskStatus,
  type TaskStore,
  type TaskRunner,
  type TaskWorker,
  type WorkerHandle,
} from "../src/index.js";
import type {
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
} from "@agentkit/contracts";

/** A clock that starts fixed and only moves when a test says so. */
export interface TestClock extends Clock {
  advance(ms: number): void;
  set(iso: string): void;
}

export function createTestClock(
  start = "2026-01-01T00:00:00.000Z",
): TestClock {
  let current = new Date(start).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms: number) => {
      current += ms;
    },
    set: (iso: string) => {
      current = new Date(iso).getTime();
    },
  };
}

/** Counter-based ids, so assertions can name the id they expect. */
export function createTestIds(): IdGenerator {
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

export class FakeConversationStore implements ConversationStore {
  readonly chats = new Map<string, ChatRecord>();
  readonly messages: MessageRecord[] = [];
  private orderKeys = new Map<string, number>();

  constructor(
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
  ) {}

  async createChat(input: CreateChatInput): Promise<ChatRecord> {
    const now = this.clock.nowIso();
    const chat: ChatRecord = {
      id: input.id ?? `chat-${this.chats.size + 1}`,
      ...(input.title === undefined ? {} : { title: input.title }),
      createdAt: now,
      updatedAt: now,
      metadata: input.metadata ?? {},
    };
    this.chats.set(chat.id, chat);
    return chat;
  }

  async getChat(chatId: string): Promise<ChatRecord | null> {
    return this.chats.get(chatId) ?? null;
  }

  async listChats(): Promise<ChatRecord[]> {
    return [...this.chats.values()];
  }

  async appendMessage(input: AppendMessageInput): Promise<MessageRecord> {
    const orderKey = (this.orderKeys.get(input.chatId) ?? 0) + 1;
    this.orderKeys.set(input.chatId, orderKey);
    const record: MessageRecord = {
      id: input.id ?? this.ids.messageId(),
      chatId: input.chatId,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      role: input.role,
      content: input.content,
      orderKey,
      ...(input.toolCallId === undefined
        ? {}
        : { toolCallId: input.toolCallId }),
      ...(input.toolCalls === undefined ? {} : { toolCalls: input.toolCalls }),
      ...(input.modelResultJson === undefined
        ? {}
        : { modelResultJson: input.modelResultJson }),
      metadata: input.metadata ?? {},
      createdAt: this.clock.nowIso(),
    };
    this.messages.push(record);
    return record;
  }

  async updateMessage(
    messageId: string,
    patch: {
      content?: string;
      metadata?: Record<string, unknown>;
      toolCalls?: AiToolCall[];
    },
  ): Promise<MessageRecord> {
    const record = this.messages.find((m) => m.id === messageId);
    if (!record) {
      throw new RecordNotFoundError(`Message not found: ${messageId}`);
    }
    if (patch.content !== undefined) record.content = patch.content;
    // Metadata REPLACES, per the port contract.
    if (patch.metadata !== undefined) record.metadata = patch.metadata;
    if (patch.toolCalls !== undefined) record.toolCalls = patch.toolCalls;
    return record;
  }

  async listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]> {
    let rows = this.messages
      .filter((m) => m.chatId === chatId)
      .sort((a, b) => a.orderKey - b.orderKey);
    if (opts?.afterOrderKey !== undefined) {
      const after = opts.afterOrderKey;
      rows = rows.filter((m) => m.orderKey > after);
    }
    if (opts?.limit !== undefined) rows = rows.slice(-opts.limit);
    return rows;
  }
}

export class FakeTaskStore implements TaskStore {
  readonly tasks = new Map<string, TaskRecord>();
  readonly attempts = new Map<string, AttemptRecord>();
  /** One live lease per task, keyed by taskId. */
  readonly leases = new Map<string, Lease>();
  readonly events = new Map<string, TaskEventEnvelope[]>();
  private fencing = 0;

  constructor(
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
  ) {}

  async createTask(input: CreateTaskInput): Promise<TaskRecord> {
    if (this.tasks.has(input.taskId)) {
      throw new DuplicateTaskError(`Task already exists: ${input.taskId}.`, {
        taskId: input.taskId,
      });
    }
    const now = this.clock.nowIso();
    const task: TaskRecord = {
      taskId: input.taskId,
      kind: input.kind,
      scopeId: input.scopeId,
      status: "queued",
      priority: input.priority ?? 0,
      enqueuedAt: now,
      availableAt: input.availableAt ?? now,
      payload: input.payload,
      attemptCount: 0,
      poisonCount: 0,
    };
    this.tasks.set(task.taskId, task);
    return task;
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    return this.tasks.get(taskId) ?? null;
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    if (!from.includes(task.status)) {
      throw new InvalidTaskTransitionError(
        `Task ${taskId} is ${task.status}, expected one of [${from.join(", ")}].`,
        { taskId, current: task.status, from, to },
      );
    }
    assertTaskTransition(task.status, to);
    task.status = to;
    if (patch?.startedAt !== undefined) task.startedAt = patch.startedAt;
    if (patch?.finishedAt !== undefined) task.finishedAt = patch.finishedAt;
    if (patch?.error !== undefined) task.error = patch.error;
    if (patch?.availableAt !== undefined) task.availableAt = patch.availableAt;
    if (patch?.priority !== undefined) task.priority = patch.priority;
    if (patch?.poisonCount !== undefined) task.poisonCount = patch.poisonCount;
    if (patch?.payload !== undefined) task.payload = patch.payload;
    return task;
  }

  async createAttempt(input: CreateAttemptInput): Promise<AttemptRecord> {
    const task = this.tasks.get(input.taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${input.taskId}`);
    task.attemptCount += 1;
    const attempt: AttemptRecord = {
      attemptId: input.attemptId,
      taskId: input.taskId,
      attemptNumber: task.attemptCount,
      status: "running",
      ownerId: input.ownerId,
      startedAt: this.clock.nowIso(),
    };
    this.attempts.set(attempt.attemptId, attempt);
    return attempt;
  }

  async endAttempt(input: EndAttemptInput): Promise<AttemptRecord> {
    const attempt = this.attempts.get(input.attemptId);
    if (!attempt) {
      throw new RecordNotFoundError(`Attempt not found: ${input.attemptId}`);
    }
    attempt.status = input.status;
    attempt.endedAt = this.clock.nowIso();
    if (input.error !== undefined) attempt.error = input.error;
    return attempt;
  }

  async acquireLease(input: AcquireLeaseInput): Promise<Lease> {
    this.fencing += 1;
    const lease: Lease = {
      taskId: input.taskId,
      attemptId: input.attemptId,
      ownerId: input.ownerId,
      leaseToken: `lease-${this.fencing}`,
      fencingToken: this.fencing,
      expiresAt: new Date(this.clock.now().getTime() + input.ttlMs).toISOString(),
    };
    this.leases.set(input.taskId, lease);
    return lease;
  }

  async renewLease(leaseToken: string, ttlMs: number): Promise<Lease> {
    const lease = this.leaseByToken(leaseToken);
    lease.expiresAt = new Date(
      this.clock.now().getTime() + ttlMs,
    ).toISOString();
    return lease;
  }

  async releaseLease(leaseToken: string): Promise<void> {
    const lease = this.leaseByToken(leaseToken);
    this.leases.delete(lease.taskId);
  }

  async expireStaleLeases(now: Date): Promise<Lease[]> {
    const expired: Lease[] = [];
    for (const [taskId, lease] of this.leases) {
      if (new Date(lease.expiresAt).getTime() <= now.getTime()) {
        expired.push(lease);
        this.leases.delete(taskId);
      }
    }
    return expired;
  }

  async appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void> {
    const lease = this.leases.get(taskId);
    if (!lease || lease.leaseToken !== opts.leaseToken) {
      throw new LeaseLostError(
        `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
        { taskId, leaseToken: opts.leaseToken },
      );
    }
    const log = this.events.get(taskId) ?? [];
    let last = log.length > 0 ? log[log.length - 1]!.seq : -1;
    for (const event of events) {
      if (event.seq <= last) {
        throw new SeqConflictError(
          `Non-monotonic seq ${event.seq} for task ${taskId} (last ${last}).`,
          { taskId, seq: event.seq, last },
        );
      }
      last = event.seq;
      log.push(event);
    }
    this.events.set(taskId, log);
  }

  async listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]> {
    let log = [...(this.events.get(taskId) ?? [])];
    if (opts?.afterSeq !== undefined) {
      const after = opts.afterSeq;
      log = log.filter((e) => e.seq > after);
    }
    if (opts?.limit !== undefined) log = log.slice(0, opts.limit);
    return log;
  }

  async nextSeq(taskId: string): Promise<number> {
    const log = this.events.get(taskId);
    if (!log || log.length === 0) return 0;
    return log[log.length - 1]!.seq + 1;
  }

  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    const busy = new Set(input.scopesBusy);
    const kinds = input.kinds === undefined ? null : new Set(input.kinds);
    const candidate = [...this.tasks.values()]
      .filter(
        (task) =>
          task.status === "queued" &&
          !busy.has(task.scopeId) &&
          (kinds === null || kinds.has(task.kind)) &&
          new Date(task.availableAt).getTime() <= input.now.getTime(),
      )
      .sort((a, b) => b.priority - a.priority)[0];
    if (!candidate) return null;
    const attempt = await this.createAttempt({
      attemptId: this.ids.attemptId(),
      taskId: candidate.taskId,
      ownerId: input.ownerId,
    });
    const lease = await this.acquireLease({
      taskId: candidate.taskId,
      attemptId: attempt.attemptId,
      ownerId: input.ownerId,
      ttlMs: 30_000,
    });
    return { task: candidate, attempt, lease };
  }

  async markDeadLettered(taskId: string, reason: string): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    task.deadLetteredAt = this.clock.nowIso();
    task.deadLetterReason = reason;
    return task;
  }

  private leaseByToken(leaseToken: string): Lease {
    for (const lease of this.leases.values()) {
      if (lease.leaseToken === leaseToken) return lease;
    }
    throw new LeaseLostError(`Unknown or expired lease token ${leaseToken}.`, {
      leaseToken,
    });
  }
}

export class FakeProposalStore implements ProposalStore {
  readonly proposals = new Map<string, ProposalRecord>();
  readonly outcomes = new Map<string, ApplyOutcome>();

  constructor(private readonly clock: Clock) {}

  async create(input: CreateProposalInput): Promise<ProposalRecord> {
    if (input.actionId !== undefined) {
      const clash = [...this.proposals.values()].find(
        (p) =>
          p.scopeKey === input.scopeKey &&
          p.actionId === input.actionId &&
          !ACTION_ID_RELEASING_STATUSES.includes(p.status),
      );
      if (clash) {
        throw new DuplicateActionIdError(
          `action_id ${input.actionId} already used in scope ${input.scopeKey}.`,
          { scopeKey: input.scopeKey, actionId: input.actionId },
        );
      }
    }
    const record: ProposalRecord = {
      id: input.id,
      chatId: input.chatId,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      scopeKey: input.scopeKey,
      ...(input.actionId === undefined ? {} : { actionId: input.actionId }),
      toolName: input.toolName,
      kind: input.kind,
      risk: input.risk,
      status: "pending",
      envelope: input.envelope,
      operations: input.operations,
      warnings: input.warnings,
      truncated: input.truncated,
      ...(input.revisionAtCreate === undefined
        ? {}
        : { revisionAtCreate: input.revisionAtCreate }),
      createdAt: input.createdAt,
    };
    this.proposals.set(record.id, record);
    return record;
  }

  async get(proposalId: string): Promise<ProposalRecord | null> {
    return this.proposals.get(proposalId) ?? null;
  }

  async getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null> {
    // Most recent wins: insertion order is creation order here, so the last
    // match is the latest attempt on that key.
    let match: ProposalRecord | null = null;
    for (const proposal of this.proposals.values()) {
      if (proposal.scopeKey === scopeKey && proposal.actionId === actionId) {
        match = proposal;
      }
    }
    return match;
  }

  async listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]> {
    return [...this.proposals.values()].filter(
      (p) =>
        p.chatId === chatId &&
        (opts?.status === undefined || p.status === opts.status),
    );
  }

  async listByStatus(status: ProposalStatus): Promise<ProposalRecord[]> {
    return [...this.proposals.values()].filter((p) => p.status === status);
  }

  async transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord> {
    const proposal = this.proposals.get(proposalId);
    if (!proposal) {
      throw new RecordNotFoundError(`Proposal not found: ${proposalId}`);
    }
    if (!from.includes(proposal.status)) {
      throw new InvalidProposalTransitionError(
        `Proposal ${proposalId} is ${proposal.status}, expected one of [${from.join(", ")}].`,
        { proposalId, current: proposal.status, from, to },
      );
    }
    assertProposalTransition(proposal.status, to);
    proposal.status = to;
    if (patch?.decision !== undefined) proposal.decision = patch.decision;
    if (patch?.decidedAt !== undefined) proposal.decidedAt = patch.decidedAt;
    if (patch?.appliedAt !== undefined) proposal.appliedAt = patch.appliedAt;
    if (patch?.operationId !== undefined) {
      proposal.operationId = patch.operationId;
    }
    if (patch?.reason !== undefined) proposal.reason = patch.reason;
    return proposal;
  }

  async recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome> {
    // Idempotent on operationId: the first outcome for an operation is the one
    // that happened, and later calls must not rewrite the evidence.
    const existing = this.outcomes.get(operationId);
    if (existing) return existing;
    this.outcomes.set(operationId, outcome);
    return outcome;
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.outcomes.get(operationId) ?? null;
  }

  async invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number> {
    let count = 0;
    for (const proposal of this.proposals.values()) {
      if (proposal.scopeKey !== scopeKey || proposal.status !== "pending") {
        continue;
      }
      assertProposalTransition(proposal.status, "invalidated");
      proposal.status = "invalidated";
      proposal.reason = `revision moved to ${newRevision}`;
      proposal.decidedAt = this.clock.nowIso();
      count++;
    }
    return count;
  }
}

export class FakeProviderStore implements ProviderStore {
  readonly providers = new Map<string, AiProviderConfig>();
  readonly models = new Map<string, AiProviderModel[]>();
  readonly capabilities = new Map<string, AiProviderCapabilities>();

  async listProviders(): Promise<AiProviderConfig[]> {
    return [...this.providers.values()];
  }
  async getProvider(providerId: string): Promise<AiProviderConfig | null> {
    return this.providers.get(providerId) ?? null;
  }
  async upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig> {
    this.providers.set(config.id, config);
    return config;
  }
  async deleteProvider(providerId: string): Promise<void> {
    this.providers.delete(providerId);
  }
  async listModels(providerId: string): Promise<AiProviderModel[]> {
    return this.models.get(providerId) ?? [];
  }
  async replaceModels(
    providerId: string,
    models: AiProviderModel[],
  ): Promise<void> {
    this.models.set(providerId, models);
  }
  async getCapabilities(
    providerId: string,
  ): Promise<AiProviderCapabilities | null> {
    return this.capabilities.get(providerId) ?? null;
  }
  async saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void> {
    this.capabilities.set(providerId, capabilities);
  }
}

export class FakeSettingsStore implements SettingsStore {
  settings: AssistantSettings = {
    contextSizePreference: "small",
    writePolicyMode: "auto_readonly_confirm_writes",
    allowRawToolData: false,
    metadata: {},
  };

  async getSettings(): Promise<AssistantSettings> {
    return this.settings;
  }

  async updateSettings(
    patch: Partial<AssistantSettings>,
  ): Promise<AssistantSettings> {
    this.settings = { ...this.settings, ...patch };
    return this.settings;
  }
}

export class FakeOutboxStore implements OutboxStore {
  readonly records: OutboxRecord[] = [];

  constructor(private readonly clock: Clock) {}

  async enqueue(input: OutboxAppendInput): Promise<OutboxRecord> {
    const now = this.clock.nowIso();
    const record: OutboxRecord = {
      id: input.id ?? `outbox-${this.records.length + 1}`,
      topic: input.topic,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      payload: input.payload,
      createdAt: now,
      availableAt: input.availableAt ?? now,
      attempts: 0,
    };
    this.records.push(record);
    return record;
  }

  async claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]> {
    return this.records
      .filter(
        (r) =>
          r.publishedAt === undefined &&
          new Date(r.availableAt).getTime() <= input.now.getTime(),
      )
      .slice(0, input.limit)
      .map((r) => {
        r.attempts += 1;
        return r;
      });
  }

  async markPublished(id: string, at: Date): Promise<void> {
    const record = this.records.find((r) => r.id === id);
    if (record) record.publishedAt = at.toISOString();
  }

  async markFailed(id: string, error: string, retryAt: Date): Promise<void> {
    const record = this.records.find((r) => r.id === id);
    if (record) {
      record.lastError = error;
      record.availableAt = retryAt.toISOString();
    }
  }
}

export class FakeAssistantStore implements AssistantStore {
  readonly conversations: FakeConversationStore;
  readonly tasks: FakeTaskStore;
  readonly proposals: FakeProposalStore;
  readonly providers = new FakeProviderStore();
  readonly settings = new FakeSettingsStore();
  readonly outbox: FakeOutboxStore;
  /** How many times `transaction` was entered — atomicity assertions read it. */
  transactions = 0;

  constructor(clock: Clock, ids: IdGenerator) {
    this.conversations = new FakeConversationStore(clock, ids);
    this.tasks = new FakeTaskStore(clock, ids);
    this.proposals = new FakeProposalStore(clock);
    this.outbox = new FakeOutboxStore(clock);
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    this.transactions += 1;
    return fn(this);
  }
}

export interface FakeApplierOptions {
  /** Outcome for the next apply; a function sees the proposal. */
  outcome?: ApplyOutcome | ((input: ApplyProposalInput) => ApplyOutcome);
  /** Throw instead of returning an outcome. */
  throws?: Error;
  /** Per-scope revisions; an absent scope reports null ("no revision info"). */
  revisions?: Record<string, string>;
}

export class FakeApplier implements ProposalApplier {
  /** Every apply that actually reached the applier — the replay assertion. */
  readonly calls: ApplyProposalInput[] = [];
  /** Outcomes the applier itself remembers, keyed by operation id. */
  readonly remembered = new Map<string, ApplyOutcome>();
  outcome: ApplyOutcome | ((input: ApplyProposalInput) => ApplyOutcome);
  throws: Error | undefined;
  revisions: Map<string, string>;

  constructor(options: FakeApplierOptions = {}) {
    this.outcome = options.outcome ?? {
      status: "applied",
      appliedOps: 1,
      failedOps: [],
    };
    this.throws = options.throws;
    this.revisions = new Map(Object.entries(options.revisions ?? {}));
  }

  async apply(input: ApplyProposalInput): Promise<ApplyOutcome> {
    this.calls.push(input);
    if (this.throws) throw this.throws;
    const outcome =
      typeof this.outcome === "function" ? this.outcome(input) : this.outcome;
    this.remembered.set(input.operationId, outcome);
    return outcome;
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.remembered.get(operationId) ?? null;
  }

  async currentRevision(scopeKey: string): Promise<string | null> {
    return this.revisions.get(scopeKey) ?? null;
  }
}

export class FakeTaskRunner implements TaskRunner {
  readonly enqueued: EnqueueInput[] = [];
  readonly cancelled: string[] = [];
  recoverCalls = 0;
  worker: TaskWorker | null = null;

  async enqueue(input: EnqueueInput): Promise<void> {
    // Idempotent per taskId, as the port requires.
    if (this.enqueued.some((e) => e.taskId === input.taskId)) return;
    this.enqueued.push(input);
  }

  async requestCancel(taskId: string): Promise<void> {
    this.cancelled.push(taskId);
  }

  async recover(): Promise<void> {
    this.recoverCalls += 1;
  }

  async startWorker(
    worker: TaskWorker,
    _opts?: StartWorkerOptions,
  ): Promise<WorkerHandle> {
    this.worker = worker;
    return { stop: async () => {} };
  }
}

/** Everything a service test needs, wired together. */
export interface TestHarness {
  clock: TestClock;
  ids: IdGenerator;
  store: FakeAssistantStore;
  applier: FakeApplier;
  taskRunner: FakeTaskRunner;
}

export function createHarness(
  applierOptions: FakeApplierOptions = {},
): TestHarness {
  const clock = createTestClock();
  const ids = createTestIds();
  return {
    clock,
    ids,
    store: new FakeAssistantStore(clock, ids),
    applier: new FakeApplier(applierOptions),
    taskRunner: new FakeTaskRunner(),
  };
}
