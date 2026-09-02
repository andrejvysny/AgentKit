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
  activationSetOf,
  activeLeafOf,
  activePathOf,
  forkedChatTitle,
  forkPrefixOf,
  assertAppendActivation,
  assertListMessagesCursors,
  hasActiveChild,
  InvalidImportError,
  isTerminalTaskStatus,
  nextBranchIndex,
  planForkedMessages,
  planImportedMessages,
  siblingsOf,
  RecordNotFoundError,
  SeqConflictError,
  UnknownDependencyError,
  assertProposalTransition,
  assertScopeIdle,
  assertTaskTransition,
  evaluateTaskDependencies,
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
  type FencedWriteOptions,
  type ForkChatResult,
  type IdGenerator,
  type ImportConversationInput,
  type Lease,
  type ListChatsOptions,
  type ListEventsOptions,
  type ListMessagesOptions,
  type ListProposalsOptions,
  type MessageRecord,
  type UpdateChatPatch,
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

export function createTestClock(start = "2026-01-01T00:00:00.000Z"): TestClock {
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
    chatId: () => next("chat"),
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
      archived: false,
    };
    this.chats.set(chat.id, chat);
    return chat;
  }

  async getChat(chatId: string): Promise<ChatRecord | null> {
    return this.chats.get(chatId) ?? null;
  }

  async listChats(opts?: ListChatsOptions): Promise<ChatRecord[]> {
    const wanted = opts?.ids === undefined ? undefined : new Set(opts.ids);
    return [...this.chats.values()].filter((chat) =>
      wanted === undefined
        ? opts?.includeArchived === true || !chat.archived
        : wanted.has(chat.id),
    );
  }

  async updateChat(
    chatId: string,
    patch: UpdateChatPatch,
  ): Promise<ChatRecord> {
    const chat = this.chats.get(chatId);
    if (!chat) throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    if (patch.title !== undefined) chat.title = patch.title;
    if (patch.metadata !== undefined) chat.metadata = patch.metadata;
    if (patch.archived !== undefined) chat.archived = patch.archived;
    chat.updatedAt = this.clock.nowIso();
    return chat;
  }

  async deleteChat(chatId: string): Promise<void> {
    if (!this.chats.has(chatId)) {
      throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    }
    for (let i = this.messages.length - 1; i >= 0; i -= 1) {
      if (this.messages[i]?.chatId === chatId) this.messages.splice(i, 1);
    }
    this.orderKeys.delete(chatId);
    this.chats.delete(chatId);
  }

  async importConversation(
    input: ImportConversationInput,
  ): Promise<ChatRecord> {
    if (this.chats.has(input.chat.id)) {
      throw new InvalidImportError(
        `Cannot import chat ${input.chat.id}: a chat with that id already exists.`,
        { reason: "duplicate_chat", chatId: input.chat.id },
      );
    }
    const createdAt = input.chat.createdAt ?? this.clock.nowIso();
    const plans = planImportedMessages(
      input.messages,
      input.chat.id,
      createdAt,
    );
    const chat: ChatRecord = {
      id: input.chat.id,
      ...(input.chat.title === undefined ? {} : { title: input.chat.title }),
      createdAt,
      updatedAt: createdAt,
      metadata: input.chat.metadata ?? {},
      archived: input.chat.archived ?? false,
    };
    this.chats.set(chat.id, chat);
    for (const plan of plans) {
      this.messages.push({
        id: plan.input.id,
        chatId: chat.id,
        role: plan.input.role,
        content: plan.input.content,
        orderKey: plan.orderKey,
        ...(plan.parentMessageId === undefined
          ? {}
          : { parentMessageId: plan.parentMessageId }),
        depth: plan.depth,
        branchIndex: plan.branchIndex,
        active: plan.input.active,
        metadata: plan.metadata,
        createdAt: plan.createdAt,
      });
    }
    this.orderKeys.set(chat.id, plans.length);
    return chat;
  }

  async appendMessage(input: AppendMessageInput): Promise<MessageRecord> {
    assertAppendActivation(input);
    const orderKey = (this.orderKeys.get(input.chatId) ?? 0) + 1;
    this.orderKeys.set(input.chatId, orderKey);
    const list = this.chatMessages(input.chatId);
    const parent =
      input.parentMessageId === undefined
        ? activeLeafOf(list)
        : this.requireParent(input.chatId, input.parentMessageId);
    const parentId = parent?.id;
    // A chain append inherits its parent's flag and moves no path — see
    // `AppendMessageInput.activate`.
    const chained = input.activate === false;
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
      ...(parentId === undefined ? {} : { parentMessageId: parentId }),
      depth: parent === undefined ? 0 : parent.depth + 1,
      branchIndex: nextBranchIndex(list, parentId),
      // A chain append inherits `active` only from a parent that is active AND
      // still the end of the live chain — see the port's `activate`.
      active: chained
        ? parent !== undefined &&
          parent.active &&
          !hasActiveChild(list, parent.id)
        : true,
      metadata: input.metadata ?? {},
      createdAt: this.clock.nowIso(),
    };
    this.messages.push(record);
    if (!chained) this.applyActivation(input.chatId, record.id);
    return record;
  }

  private chatMessages(chatId: string): MessageRecord[] {
    return this.messages.filter((m) => m.chatId === chatId);
  }

  private requireParent(
    chatId: string,
    parentMessageId: string,
  ): MessageRecord {
    const parent = this.messages.find(
      (m) => m.id === parentMessageId && m.chatId === chatId,
    );
    if (!parent) {
      throw new RecordNotFoundError(
        `Parent message not found in chat ${chatId}: ${parentMessageId}`,
      );
    }
    return parent;
  }

  private applyActivation(chatId: string, messageId: string): void {
    const list = this.chatMessages(chatId);
    const active = activationSetOf(list, messageId);
    for (const record of list) record.active = active.has(record.id);
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
    assertListMessagesCursors(opts);
    let rows = activePathOf(this.chatMessages(chatId));
    if (opts?.afterOrderKey !== undefined) {
      const after = opts.afterOrderKey;
      rows = rows.filter((m) => m.orderKey > after);
    }
    if (opts?.beforeOrderKey !== undefined) {
      const before = opts.beforeOrderKey;
      rows = rows.filter((m) => m.orderKey < before);
    }
    if (opts?.limit !== undefined) rows = rows.slice(-opts.limit);
    return rows;
  }

  /** The deepest record a run wrote — `(depth, orderKey)` desc, see the port. */
  async lastMessageOfRun(
    chatId: string,
    runId: string,
  ): Promise<MessageRecord | null> {
    const written = this.chatMessages(chatId).filter(
      (record) => record.runId === runId,
    );
    let deepest: MessageRecord | undefined;
    for (const record of written) {
      if (
        deepest === undefined ||
        record.depth > deepest.depth ||
        (record.depth === deepest.depth && record.orderKey > deepest.orderKey)
      ) {
        deepest = record;
      }
    }
    return deepest ?? null;
  }

  async listSiblings(messageId: string): Promise<MessageRecord[]> {
    const record = this.requireMessage(messageId);
    return siblingsOf(this.chatMessages(record.chatId), record);
  }

  async activatePath(messageId: string): Promise<MessageRecord[]> {
    const record = this.requireMessage(messageId);
    this.applyActivation(record.chatId, messageId);
    return activePathOf(this.chatMessages(record.chatId));
  }

  async forkChat(
    chatId: string,
    fromMessageId: string,
  ): Promise<ForkChatResult> {
    const source = this.chats.get(chatId);
    if (!source) throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    const prefix = forkPrefixOf(
      this.chatMessages(chatId),
      chatId,
      fromMessageId,
    );
    const plans = planForkedMessages(prefix, () => this.ids.messageId());
    const now = this.clock.nowIso();
    const title = forkedChatTitle(source.title);
    const chat: ChatRecord = {
      id: `chat-fork-${this.chats.size + 1}`,
      ...(title === undefined ? {} : { title }),
      createdAt: now,
      updatedAt: now,
      metadata: { ...source.metadata },
      archived: false,
    };
    const messages: MessageRecord[] = plans.map((plan, index) => ({
      id: plan.id,
      chatId: chat.id,
      role: plan.source.role,
      content: plan.source.content,
      orderKey: index + 1,
      ...(plan.source.toolCallId === undefined
        ? {}
        : { toolCallId: plan.source.toolCallId }),
      ...(plan.source.toolCalls === undefined
        ? {}
        : { toolCalls: plan.source.toolCalls }),
      ...(plan.source.modelResultJson === undefined
        ? {}
        : { modelResultJson: plan.source.modelResultJson }),
      ...(plan.parentMessageId === undefined
        ? {}
        : { parentMessageId: plan.parentMessageId }),
      depth: plan.depth,
      branchIndex: 0,
      active: true,
      metadata: plan.metadata,
      createdAt: now,
    }));
    this.chats.set(chat.id, chat);
    this.messages.push(...messages);
    this.orderKeys.set(chat.id, messages.length);
    return { chat, messages };
  }

  private requireMessage(messageId: string): MessageRecord {
    const record = this.messages.find((m) => m.id === messageId);
    if (!record) {
      throw new RecordNotFoundError(`Message not found: ${messageId}`);
    }
    return record;
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
    // After the duplicate check, exactly as both reference adapters order it —
    // a fake that refused a redelivery as `chat_busy` would let a submit
    // idempotency bug pass every test here.
    if (input.exclusiveScope === true) {
      assertScopeIdle(
        input.scopeId,
        [...this.tasks.values()].filter(
          (task) =>
            task.scopeId === input.scopeId &&
            !isTerminalTaskStatus(task.status),
        ),
      );
    }
    // Deps must pre-exist, exactly as the real adapters demand — a fake that
    // accepted a dangling edge would let a service test build a graph no store
    // would ever have stored.
    if (
      input.parentTaskId !== undefined &&
      !this.tasks.has(input.parentTaskId)
    ) {
      throw new UnknownDependencyError(
        `Task ${input.taskId} names a parent that does not exist: ${input.parentTaskId}.`,
        { taskId: input.taskId, parentTaskId: input.parentTaskId },
      );
    }
    for (const dependency of input.dependsOn ?? []) {
      if (dependency === input.taskId || !this.tasks.has(dependency)) {
        throw new UnknownDependencyError(
          `Task ${input.taskId} depends on ${dependency}, which does not exist.`,
          { taskId: input.taskId, dependsOn: dependency },
        );
      }
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
      ...(input.parentTaskId === undefined
        ? {}
        : { parentTaskId: input.parentTaskId }),
      ...(input.dependsOn === undefined
        ? {}
        : { dependsOn: [...input.dependsOn] }),
      attemptCount: 0,
      poisonCount: 0,
    };
    this.tasks.set(task.taskId, task);
    return task;
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    return this.tasks.get(taskId) ?? null;
  }

  async listChildren(taskId: string): Promise<TaskRecord[]> {
    return [...this.tasks.values()].filter(
      (task) => task.parentTaskId === taskId,
    );
  }

  async listByScope(scopeId: string): Promise<TaskRecord[]> {
    return [...this.tasks.values()].filter((task) => task.scopeId === scopeId);
  }

  async deleteByScope(scopeId: string): Promise<number> {
    const doomed = [...this.tasks.values()].filter(
      (task) => task.scopeId === scopeId,
    );
    for (const task of doomed) {
      for (const [attemptId, attempt] of this.attempts) {
        if (attempt.taskId === task.taskId) this.attempts.delete(attemptId);
      }
      this.leases.delete(task.taskId);
      this.events.delete(task.taskId);
      this.tasks.delete(task.taskId);
    }
    return doomed.length;
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    if (opts?.leaseToken !== undefined) {
      this.assertLeaseCurrent(taskId, opts.leaseToken);
    }
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
    if (input.leaseToken !== undefined) {
      this.assertLeaseCurrent(attempt.taskId, input.leaseToken);
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
      expiresAt: new Date(
        this.clock.now().getTime() + input.ttlMs,
      ).toISOString(),
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

  async updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: { leaseToken: string },
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    const lease = this.leases.get(taskId);
    if (!lease || lease.leaseToken !== opts.leaseToken) {
      throw new LeaseLostError(
        `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
        { taskId, leaseToken: opts.leaseToken },
      );
    }
    task.progress = progress;
    return task;
  }

  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    const busy = new Set(input.scopesBusy);
    const kinds = input.kinds === undefined ? null : new Set(input.kinds);
    const candidates = [...this.tasks.values()]
      .filter(
        (task) =>
          task.status === "queued" &&
          !busy.has(task.scopeId) &&
          (kinds === null || kinds.has(task.kind)) &&
          new Date(task.availableAt).getTime() <= input.now.getTime(),
      )
      .sort((a, b) => b.priority - a.priority);
    for (const candidate of candidates) {
      // The dependency gate is queue semantics, so the fake enforces it too —
      // see TaskStore.claimNext. Bad dependencies settle the dependent in place
      // and the scan moves on.
      const verdict = evaluateTaskDependencies(
        (candidate.dependsOn ?? []).map((dependencyId) => {
          const dependency = this.tasks.get(dependencyId);
          return {
            taskId: dependencyId,
            status: dependency?.status ?? null,
            deadLettered: dependency?.deadLetteredAt !== undefined,
          };
        }),
      );
      if (verdict.kind === "blocked") continue;
      if (verdict.kind === "settle") {
        await this.transitionTask(candidate.taskId, ["queued"], verdict.to, {
          finishedAt: this.clock.nowIso(),
          ...(verdict.error === undefined ? {} : { error: verdict.error }),
        });
        continue;
      }
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
    return null;
  }

  async markDeadLettered(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    if (opts?.leaseToken !== undefined) {
      this.assertLeaseCurrent(taskId, opts.leaseToken);
    }
    task.deadLetteredAt = this.clock.nowIso();
    task.deadLetterReason = reason;
    return task;
  }

  /**
   * The fence the real adapters apply inside their own transaction. Enforced
   * here too, or a host test could not tell a write that proved ownership from
   * one that only claimed it — which is the whole point of the option.
   */
  private assertLeaseCurrent(taskId: string, leaseToken: string): void {
    const lease = this.leases.get(taskId);
    if (!lease || lease.leaseToken !== leaseToken) {
      throw new LeaseLostError(
        `Lease token ${leaseToken} is not current for task ${taskId}.`,
        { taskId, leaseToken },
      );
    }
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

  async deleteByChat(chatId: string): Promise<number> {
    const doomed = [...this.proposals.values()].filter(
      (proposal) => proposal.chatId === chatId,
    );
    for (const proposal of doomed) {
      if (proposal.operationId !== undefined) {
        this.outcomes.delete(proposal.operationId);
      }
      this.proposals.delete(proposal.id);
    }
    return doomed.length;
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

  /** Published rows only: this fake has no attempt cap to exhaust. */
  async prune(before: Date): Promise<number> {
    const doomed = this.records.filter(
      (r) =>
        r.publishedAt !== undefined &&
        new Date(r.publishedAt).getTime() < before.getTime(),
    );
    for (const record of doomed) {
      this.records.splice(this.records.indexOf(record), 1);
    }
    return doomed.length;
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
