/**
 * The wiring for a standalone coding agent: AgentKit's host, the sqlite store,
 * the local task runner and the coding tool pack, with no Pi and no OneMind in
 * sight. The CLI in `main.ts` is a thin shell over this; the acceptance test
 * drives the same object with a scripted provider.
 *
 * What the host owns, and what this file refuses to reimplement:
 *
 *  - the agent loop is `@agentkit/core`'s — this file never calls a provider;
 *  - scheduling, leases and crash recovery are `TaskStore`/`TaskRunner`;
 *  - sessions, follow-ups, steering and approvals are `AgentSessionService`;
 *  - permission is the coding guard plus the session's durable execution state,
 *    so a mode is a fact in the database, not a variable a running turn could
 *    talk its way into.
 */
import { mkdirSync } from "node:fs";
import { dirname, isAbsolute, resolve } from "node:path";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import type { AiRunEvent, TaskEventEnvelope } from "@agentkit/contracts";
import { OpenAiCompatibleClient, type AiProviderClient } from "@agentkit/core";
import {
  AgentSessionService,
  ChatTurnExecutor,
  ExecutorRegistry,
  PROVIDER_SECRET_REF_KEY,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  createProfilePermissionAuthority,
  defaultClock,
  defaultIds,
  defaultProfileRegistry,
  readSessionDescriptor,
  type AgentInputRecord,
  type AgentSessionExport,
  type AgentSessionState,
  type AgentSessionUsage,
  type AgentTurnSummary,
  type ProfileRegistry,
  type SessionReasoningEffort,
  type SecretStore,
  type TaskStatus,
  type ToolSetContributor,
  type WorkerHandle,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import {
  SUBMIT_PLAN_TOOL_NAME,
  createCodingPermissionGuard,
  createCodingToolPack,
  type CodingMode,
} from "@agentkit/toolpacks-coding";

const TERMINAL: ReadonlySet<TaskStatus> = new Set([
  "completed",
  "failed",
  "cancelled",
]);

/** An env-var-backed secret store: the key never enters the database. */
export function envSecretStore(
  env: NodeJS.ProcessEnv = process.env,
): SecretStore {
  return {
    async get(ref) {
      const value = env[ref];
      return typeof value === "string" && value !== "" ? value : null;
    },
    async set(ref, secret) {
      env[ref] = secret;
    },
    async delete(ref) {
      delete env[ref];
    },
    // An env-backed store cannot enumerate the process environment as if it were
    // an inventory of this app's secrets: the list would be every variable the
    // machine has, most of them nobody's credentials.
    async listRefs() {
      return [];
    },
  };
}

export interface CodingAgentOptions {
  /** The repository to work in. Explicit, absolute, and the only root granted. */
  repo: string;
  /** SQLite file for sessions, tasks and transcripts. */
  dbPath: string;
  sessionId: string;
  /** A profile id; see `defaultProfileRegistry()`. */
  profileId: string;
  model: string;
  /** Applied with `setModel` on a new session, or when it differs on an existing one. */
  reasoningEffort?: SessionReasoningEffort;
  baseUrl: string;
  /** Name of the environment variable holding the API key. */
  apiKeyEnv?: string;
  providerId?: string;
  /** Permission mode to start the session in; the profile's wins if unset. */
  mode?: CodingMode;
  /** Commands `run_tests` may run. Default ["bun test"]. */
  testCommands?: readonly string[];
  /** Injected by the acceptance test; production uses the real client. */
  providerFactory?: (config: {
    baseUrl: string;
    defaultModel: string;
    apiKey?: string;
  }) => AiProviderClient;
  /** Extra tool contributors (a host's own domain tools). */
  extraContributors?: readonly ToolSetContributor[];
  profiles?: ProfileRegistry;
  leaseTtlMs?: number;
  maxToolIterations?: number;
  quiet?: boolean;
}

export interface PendingApproval {
  requestId: string;
  answerId: string;
  toolName: string;
  /** SHA-256 of the exact arguments the approval is bound to. */
  argsSha256?: string;
  prompt: string;
  /** Set when the request is a plan the user is asked to approve as a whole. */
  plan?: { planDigest: string; plan?: string; requestedMode?: string };
}

/** One row of `--list-sessions`. */
export interface SessionListing {
  sessionId: string;
  profileId: string;
  model?: string;
  reasoningEffort?: SessionReasoningEffort;
  turns: number;
  lastTurnStatus?: TaskStatus;
}

export class CodingAgentApp {
  private constructor(
    readonly options: CodingAgentOptions,
    readonly store: SqliteAssistantStore,
    readonly taskRunner: SingleProcessTaskRunner,
    readonly service: AgentSessionService,
    readonly profiles: ProfileRegistry,
    private readonly registry: ExecutorRegistry,
    private worker: WorkerHandle | null,
  ) {}

  /**
   * Open (creating if needed) everything the agent needs. Nothing here starts a
   * model call: opening an app must not resume work by itself, or a crash loop
   * would become a model-call loop.
   */
  static async open(options: CodingAgentOptions): Promise<CodingAgentApp> {
    if (!isAbsolute(options.repo))
      throw new Error(`repo must be an absolute path, got "${options.repo}"`);
    if (!isAbsolute(options.dbPath))
      throw new Error("dbPath must be an absolute path");
    mkdirSync(dirname(options.dbPath), { recursive: true });
    const providerId = options.providerId ?? "local";
    const profiles = options.profiles ?? defaultProfileRegistry();
    const profile = profiles.resolve(options.profileId);
    if (profile === undefined) {
      throw new Error(
        `Unknown profile "${options.profileId}". Known: ${profiles
          .list()
          .map((p) => p.id)
          .join(", ")}.`,
      );
    }

    // ONE lease policy for both halves of the scheduler: the store grants the
    // lease when it claims a task, the runner renews it. Setting them apart is
    // how a host gets a heartbeat that outlives the lease it is renewing.
    const leaseTtlMs = options.leaseTtlMs ?? 30_000;
    const store = new SqliteAssistantStore(options.dbPath, { leaseTtlMs });
    await store.providers.upsertProvider({
      id: providerId,
      label: `OpenAI-compatible (${options.baseUrl})`,
      kind: "openai-compatible",
      baseUrl: options.baseUrl,
      defaultModel: options.model,
      enabled: true,
      ...(options.apiKeyEnv === undefined
        ? {}
        : {
            metadata: { [PROVIDER_SECRET_REF_KEY]: options.apiKeyEnv },
          }),
    });
    await store.settings.updateSettings({ defaultProviderId: providerId });

    const taskRunner = new SingleProcessTaskRunner({
      store,
      leaseTtlMs,
      heartbeatMs: Math.max(250, Math.floor(leaseTtlMs / 3)),
      pollMs: 50,
      // Manual + interrupt: opening never restarts work, and closing parks a
      // running turn `interrupted` so only an explicit resume continues it.
      recoveryMode: "manual",
      shutdownMode: "interrupt",
    });
    // The profile and the session's durable execution state decide permission;
    // `options.mode` is the floor for a session that has said nothing yet.
    const authority = createProfilePermissionAuthority<CodingMode>({
      store,
      profiles,
      ...(options.mode === undefined ? {} : { fallbackMode: options.mode }),
    });
    const contributors: ToolSetContributor[] = [
      createCodingToolPack({
        workspace: { root: resolve(options.repo) },
        testCommands: options.testCommands ?? ["bun test"],
        permission: authority,
      }),
      ...(options.extraContributors ?? []),
    ];
    const turnRunner = new TurnRunner({
      store,
      taskRunner,
      providerFactory: (config) =>
        options.providerFactory
          ? options.providerFactory({
              baseUrl: config.baseUrl,
              defaultModel: config.defaultModel,
              ...(config.apiKey === undefined ? {} : { apiKey: config.apiKey }),
            })
          : OpenAiCompatibleClient.fromConfig(config),
      contributors,
      // ONE guard, consulting the host-owned authority: the model never chooses
      // its own mode, and a role ceiling stays final under every mode.
      toolGuards: [
        createCodingPermissionGuard({
          mode: options.mode ?? "ask",
          authority,
        }),
      ],
      profiles,
      secrets: envSecretStore(),
      ...(options.maxToolIterations === undefined
        ? {}
        : { maxToolIterations: options.maxToolIterations }),
      clock: defaultClock,
      ids: defaultIds,
    });
    const tasks = new TaskService({
      store,
      taskRunner,
      ids: defaultIds,
      clock: defaultClock,
    });
    const service = new AgentSessionService({
      store,
      turns: turnRunner,
      tasks,
      taskRunner,
      clock: defaultClock,
      ids: defaultIds,
      // A plan approval is the only way out of plan mode, and the HOST decides
      // what it grants. The model's requested mode is recorded, never obeyed —
      // and the granted mode can never be plan itself, so a session cannot be
      // escalated into the mode that blocks it.
      planApproval: {
        toolNames: [SUBMIT_PLAN_TOOL_NAME],
        grantedMode:
          options.mode === undefined || options.mode === "plan"
            ? "auto_edit"
            : options.mode,
      },
    });
    const registry = new ExecutorRegistry();
    registry.register(new ChatTurnExecutor(turnRunner));
    return new CodingAgentApp(
      options,
      store,
      taskRunner,
      service,
      profiles,
      registry,
      null,
    );
  }

  async ensureSession(): Promise<AgentSessionState> {
    const existing = await this.store.conversations.getChat(
      this.options.sessionId,
    );
    if (existing !== null) {
      const { descriptor } = await this.service.getState(
        this.options.sessionId,
      );
      const effort = this.options.reasoningEffort;
      if (effort === undefined || descriptor.reasoningEffort === effort)
        return this.service.getState(this.options.sessionId);
      return this.service.setModel(this.options.sessionId, {
        model: descriptor.model ?? this.options.model,
        reasoningEffort: effort,
      });
    }
    const created = await this.service.createSession({
      sessionId: this.options.sessionId,
      profile: { id: this.options.profileId },
      providerId: this.options.providerId ?? "local",
      model: this.options.model,
      workspace: { root: resolve(this.options.repo), trusted: true },
      context: { policy: "transcript" },
      ...(this.options.mode === undefined
        ? {}
        : { executionMode: this.options.mode }),
    });
    const effort = this.options.reasoningEffort;
    // createSession has no effort field; setModel is the one place it is set.
    return effort === undefined
      ? created
      : this.service.setModel(this.options.sessionId, {
          model: this.options.model,
          reasoningEffort: effort,
        });
  }

  async startWorker(): Promise<void> {
    if (this.worker) return;
    // A manual-recovery runner refuses to claim until it has reconciled boot:
    // anything left queued by a previous process is parked `interrupted`
    // rather than silently restarted.
    await this.recover();
    this.worker = await this.taskRunner.startWorker(
      createDispatchingWorker(this.registry, {
        store: this.store,
        clock: defaultClock,
      }),
      { concurrency: 1, ownerId: `coding-agent-${process.pid}` },
    );
  }

  async stopWorker(): Promise<void> {
    await this.worker?.stop();
    this.worker = null;
  }

  /**
   * Park work a previous process left behind as `interrupted`. This never
   * restarts it — `resumeTurn` does, on explicit intent — and a resumed turn
   * never re-runs a tool whose outcome the log cannot decide (see side-effects).
   * A killed process's lease must expire first, so repeat until it is parked.
   */
  async recover(): Promise<void> {
    await this.taskRunner.recover();
  }

  /**
   * An instruction. Typing while a turn runs queues it as a follow-up rather
   * than losing it, so every input needs an id — mint one when the caller has
   * no opinion, and the queue stays deduplicated and idempotent on retry.
   */
  async send(text: string, inputId?: string) {
    return this.service.send(this.options.sessionId, {
      content: text,
      inputId: inputId ?? `input-${defaultIds.messageId()}`,
      whenBusy: "follow_up",
    });
  }

  async followUp(text: string, inputId: string) {
    return this.service.followUp(this.options.sessionId, {
      inputId,
      content: text,
    });
  }

  async steer(text: string, inputId: string) {
    return this.service.steer(this.options.sessionId, {
      inputId,
      content: text,
    });
  }

  async state(): Promise<AgentSessionState> {
    return this.service.getState(this.options.sessionId);
  }

  async status(turnId: string): Promise<TaskStatus | undefined> {
    return (await this.store.tasks.getTask(turnId))?.status;
  }

  /** The approval this session is parked on, if any. */
  async pendingApproval(): Promise<PendingApproval | undefined> {
    const state = await this.state();
    const request: AgentInputRecord | undefined = state.waiting;
    if (request === undefined) return undefined;
    const content = request.content as { prompt?: unknown };
    return {
      requestId: request.inputId,
      // Stable, so a retry of the same decision is the same answer, and a
      // DIFFERENT decision is refused rather than silently overriding a human.
      answerId: `ans:${request.inputId}`,
      toolName: state.waitingApproval?.toolName ?? "tool",
      ...(state.waitingApproval === undefined
        ? {}
        : { argsSha256: state.waitingApproval.argsSha256 }),
      prompt:
        typeof content?.prompt === "string"
          ? content.prompt
          : (state.waitingApproval?.prompt ?? ""),
      ...(state.waitingPlan === undefined
        ? {}
        : {
            plan: {
              planDigest: state.waitingPlan.planDigest,
              ...(state.waitingPlan.plan === undefined
                ? {}
                : { plan: state.waitingPlan.plan }),
              ...(state.waitingPlan.requestedMode === undefined
                ? {}
                : { requestedMode: state.waitingPlan.requestedMode }),
            },
          }),
    };
  }

  async resolveApproval(
    approval: PendingApproval,
    decision: "approve" | "deny",
    note?: string,
  ): Promise<void> {
    if (decision === "approve")
      await this.service.approve(approval.requestId, approval.answerId, note);
    else await this.service.deny(approval.requestId, approval.answerId, note);
  }

  /** The durable execution mode the session is in, after any plan approval. */
  async executionMode(): Promise<string | undefined> {
    return (await this.service.getExecution(this.options.sessionId))?.mode;
  }

  async setExecutionMode(mode: string): Promise<void> {
    await this.service.setExecutionMode(this.options.sessionId, mode);
  }

  /** Park a running turn `interrupted`; `resumeTurn` continues it. */
  async interrupt(turnId: string): Promise<void> {
    await this.service.interrupt(this.options.sessionId, turnId);
  }

  /** The session's interrupted turn, if it has one. */
  async interruptedTurn(): Promise<string | undefined> {
    const active = (await this.state()).activeTurn;
    return active?.status === "interrupted" ? active.turnId : undefined;
  }

  async resumeTurn(turnId: string): Promise<void> {
    await this.service.resume(this.options.sessionId, turnId);
  }

  async turns(): Promise<AgentTurnSummary[]> {
    return this.service.listTurns(this.options.sessionId);
  }

  async usage(): Promise<AgentSessionUsage> {
    return this.service.getUsage(this.options.sessionId);
  }

  async exportSession(): Promise<AgentSessionExport> {
    return this.service.exportSession(this.options.sessionId);
  }

  /** Takes effect on the NEXT turn; a turn already running keeps its request. */
  async setModel(
    model: string,
    reasoningEffort?: SessionReasoningEffort | null,
  ): Promise<AgentSessionState> {
    return this.service.setModel(this.options.sessionId, {
      model,
      ...(reasoningEffort === undefined ? {} : { reasoningEffort }),
    });
  }

  /** Every agent session in the database, newest first. */
  async listSessions(): Promise<SessionListing[]> {
    const listings: SessionListing[] = [];
    let before: string | undefined;
    for (;;) {
      const page = await this.store.conversations.listChats({
        limit: 100,
        includeArchived: true,
        ...(before === undefined ? {} : { before }),
      });
      if (page.length === 0) break;
      for (const chat of page) {
        const descriptor = readSessionDescriptor(chat.metadata);
        if (descriptor === undefined) continue;
        const turns = await this.service.listTurns(chat.id);
        const last = turns[turns.length - 1];
        listings.push({
          sessionId: chat.id,
          profileId: descriptor.profile.id,
          ...(descriptor.model === undefined
            ? {}
            : { model: descriptor.model }),
          ...(descriptor.reasoningEffort === undefined
            ? {}
            : { reasoningEffort: descriptor.reasoningEffort }),
          turns: turns.length,
          ...(last === undefined ? {} : { lastTurnStatus: last.status }),
        });
      }
      if (page.length < 100) break;
      before = page[page.length - 1]!.updatedAt;
    }
    return listings;
  }

  async cancel(turnId: string): Promise<void> {
    await this.service.cancel(this.options.sessionId, turnId);
  }

  /**
   * The transcript, oldest first. Tool results are internal records (the model
   * sees them, a chat UI usually does not) but a coding agent's log is not a
   * chat log: what the agent read, changed and ran IS the work, so they stay.
   * The empty assistant stubs the loop writes around each call do not.
   */
  async transcript(): Promise<
    { role: string; content: string; toolName?: string }[]
  > {
    const messages = await this.store.conversations.listMessages(
      this.options.sessionId,
    );
    return messages
      .filter(
        (m) => !(m.role === "assistant" && m.metadata["internal"] === true),
      )
      .map((m) => ({
        role: m.role,
        content: typeof m.content === "string" ? m.content : "",
        ...(typeof m.metadata["toolName"] === "string"
          ? { toolName: m.metadata["toolName"] as string }
          : {}),
      }));
  }

  /** Events of a turn from `afterSeq` on — the CLI's live activity feed. */
  async eventsAfter(
    turnId: string,
    afterSeq: number,
  ): Promise<{ events: AiRunEvent[]; nextSeq: number }> {
    const all = (await this.store.tasks.listEvents(
      turnId,
    )) as unknown as TaskEventEnvelope[];
    const events = all.filter(
      (e) => e.seq > afterSeq,
    ) as unknown as AiRunEvent[];
    return {
      events,
      nextSeq: events.length === 0 ? afterSeq : events[events.length - 1]!.seq,
    };
  }

  async waitForTurn(
    turnId: string,
    options: {
      /** Asked whenever the turn parks; return the human's decision. */
      decide?: (approval: PendingApproval) => Promise<"approve" | "deny">;
      /** Live activity, once per newly appended event. */
      onEvent?: (event: AiRunEvent) => void;
      timeoutMs?: number;
      pollMs?: number;
    } = {},
  ): Promise<TaskStatus> {
    const deadline = Date.now() + (options.timeoutMs ?? 10 * 60_000);
    const seen = new Set<string>();
    let afterSeq = -1;
    for (;;) {
      const { events, nextSeq } = await this.eventsAfter(turnId, afterSeq);
      afterSeq = nextSeq;
      for (const event of events) {
        if (seen.has(event.eventId)) continue;
        seen.add(event.eventId);
        options.onEvent?.(event);
      }
      const status = await this.status(turnId);
      if (status === undefined) throw new Error(`unknown turn ${turnId}`);
      if (TERMINAL.has(status)) return status;
      if (status === "waiting_approval") {
        const approval = await this.pendingApproval();
        if (approval !== undefined) {
          const decision = options.decide
            ? await options.decide(approval)
            : "deny";
          await this.resolveApproval(approval, decision);
        }
      }
      if (Date.now() > deadline)
        throw new Error(`turn ${turnId} did not finish within its budget`);
      await new Promise((r) => setTimeout(r, options.pollMs ?? 100));
    }
  }

  async close(): Promise<void> {
    await this.stopWorker();
    this.store.close();
  }
}
