/**
 * The wiring for a standalone coding agent on the NATIVE composition: the
 * Rust runtime owns sessions, storage, scheduling, governance, durable
 * execution and recovery — and, since `registerCodingTools`, the coding
 * tools themselves: the Rust toolpack's descriptors, bodies, permission
 * guard, high-risk guard, and fingerprint-v4 standing approvals all live
 * on the same executor lane as the (optional) JS tools. No Pi, no OneMind,
 * and no TS coding pack in sight.
 *
 * What the runtime owns, and what this file refuses to reimplement:
 *
 *  - the agent loop, the scheduler, leases and crash reconciliation are
 *    Rust's — this file never calls a provider directly and never parks or
 *    adopts a turn itself;
 *  - sessions, follow-ups, steering and approvals are `NativeSessionService`;
 *  - permission is the Rust coding-pack authority over each tool's
 *    effect+capability (JS tools via the binding's JS-tool guard, native
 *    tools via the crate's own guard chain), so a mode is a fact in the
 *    database, not a variable a running turn could talk its way into;
 *  - the JS provider callback is the only code that crosses into this
 *    process (the JS tool callbacks remain available for host tools), and
 *    both are settled honestly by the binding — never fabricated.
 */
import { mkdirSync } from "node:fs";
import { dirname, isAbsolute, resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";
import type {
  AiChatMessage,
  AiRunEvent,
  AiToolDefinition,
} from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type { SessionReasoningEffort } from "@agentkit/host";
import {
  NativeSessionService,
  NativeStore,
  NativeAgentError,
  listNativeProfiles,
  type CreateSessionInputJson,
  type NativeAgentProfile,
  type NativeResolvedAgentProfile,
  type ProviderRequest,
  type ProviderResult,
} from "@agentkit/native";

/** The tool whose approvable refusal is a PLAN approval — the same name
 * the Rust pack registers, spelled locally so nothing runtime crosses in
 * from the TS pack. */
const SUBMIT_PLAN_TOOL_NAME = "submit_plan";

const TERMINAL: Readonly<Record<string, true>> = {
  completed: true,
  failed: true,
  cancelled: true,
};

type NativeMode = "read_only" | "ask" | "auto_edit" | "full_auto" | "plan";
const NATIVE_MODES: Readonly<Record<string, true>> = {
  read_only: true,
  ask: true,
  auto_edit: true,
  full_auto: true,
  plan: true,
};

/** The permission modes the native runtime understands — the same words
 * the Rust coding pack and the binding's JS-tool guard parse. Spelled
 * locally so no runtime import crosses from the TS pack. */
export type CodingMode = NativeMode;

/** The durable status of one turn, as the native scheduler records it. */
export type TurnStatus =
  | "queued"
  | "running"
  | "interrupted"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

/** The session's durable facts, as the native session service projects them. */
export interface NativeSessionState {
  sessionId: string;
  descriptor: {
    version: number;
    profile: { id: string; digest?: string };
    profileSnapshot?: Omit<NativeResolvedAgentProfile, "digest">;
    contentSelection?: CreateSessionInputJson["contentSelection"];
    model?: string;
    reasoningEffort?: SessionReasoningEffort;
    workspace?: { root: string; trusted?: boolean };
  };
  activeTurn: { turnId: string; status: TurnStatus } | null;
  pending: unknown[];
  waiting: {
    inputId: string;
    content?: Record<string, unknown>;
  } | null;
  waitingApproval?: {
    requestId: string;
    toolName: string;
    argsSha256?: string;
    prompt?: string;
  } | null;
  waitingPlan?: {
    plan?: string;
    planDigest?: string;
    requestedMode?: string;
  } | null;
}

export interface AgentTurnSummary {
  turnId: string;
  status: TurnStatus;
  attempts: number;
  createdAt: number;
  finishedAt?: number;
  model?: string;
  error?: string;
}

export interface AgentSessionUsage {
  sessionId: string;
  turns: {
    turnId: string;
    requests: number;
    promptTokens: number;
    completionTokens: number;
    totalTokens: number;
  }[];
  totals: {
    requests: number;
    promptTokens: number;
    completionTokens: number;
    totalTokens: number;
  };
  contextWindowTokens: number;
  contextWindowSource: string;
  lastPromptTokens?: number;
}

export interface CodingAgentOptions {
  /** The repository to work in. Explicit, absolute, and the only root granted. */
  repo: string;
  /** SQLite file for the native store: sessions, tasks, transcripts, receipts. */
  dbPath: string;
  sessionId: string;
  /** Resolved and frozen by the native profile registry. */
  profileId: string;
  profiles?: readonly NativeAgentProfile[];
  /** Immutable template/skill selection for a new session. */
  contentSelection?: CreateSessionInputJson["contentSelection"];
  /** Host-selected connector grants; native content assembly remains authoritative. */
  contentAuthority?: {
    authorize: NonNullable<
      Parameters<NativeSessionService["setContentAuthority"]>[0]
    >;
    identity?: NonNullable<
      Parameters<NativeSessionService["setContentAuthority"]>[1]
    >;
  };
  model: string;
  /** Recorded atomically at creation; changed explicitly on an existing session. */
  reasoningEffort?: SessionReasoningEffort;
  /** OpenAI-compatible endpoint for the Rust provider lane. Required when no
   * `providerFactory` is injected; ignored by the injected (JS) provider lane. */
  baseUrl?: string;
  /** Name of the environment variable holding the API key. The key crosses
   * into the runtime only through `registerProviderConnection` and never
   * enters the database. */
  apiKeyEnv?: string;
  /** Permission mode to start the session in; the profile's wins if unset. */
  mode?: CodingMode;
  /** Commands `run_tests` may run. Default ["bun test"]. */
  testCommands?: readonly string[];
  /** Execution backend for shell/test commands. Default "required": commands
   * only run inside the platform sandbox, and hosts without one (Linux has no
   * `sandbox-exec`) refuse. "off" runs unsandboxed — an explicit opt-in for
   * trusted hosts and CI, never the default. */
  sandbox?: "required" | "off";
  /** Injected by the acceptance test; production uses the Rust provider lane
   * (an OpenAI-compatible connection over `baseUrl` + `apiKeyEnv`). */
  providerFactory?: (config: {
    baseUrl?: string;
    defaultModel: string;
    apiKey?: string;
  }) => AiProviderClient;
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
  lastTurnStatus?: TurnStatus;
}

/** Collapse one scripted/streamed provider answer into the single final
 * result the binding's JS provider lane expects (the runtime synthesizes
 * the durable provider event stream from it). A failed run is a thrown
 * error — never a fabricated success. The fixed client remains a host extension. */
async function finalResult(
  client: AiProviderClient,
  request: ProviderRequest,
): Promise<ProviderResult> {
  await request.checkAuthority?.();
  const input: AiChatRequest = {
    runId: request.runId,
    model: request.model,
    messages: request.messages as AiChatMessage[],
    ...(request.signal === undefined ? {} : { signal: request.signal }),
    ...(request.checkAuthority === undefined
      ? {}
      : { beforeRequest: request.checkAuthority }),
    ...(request.reasoningEffort == null
      ? {}
      : { reasoningEffort: request.reasoningEffort }),
    ...(request.runContent === undefined
      ? {}
      : { runContent: request.runContent as AiChatRequest["runContent"] }),
    ...(request.tools.length > 0
      ? { tools: request.tools as AiToolDefinition[] }
      : {}),
    ...(request.temperature === null || request.temperature === undefined
      ? {}
      : { temperature: request.temperature }),
    ...(request.maxOutputTokens === null ||
    request.maxOutputTokens === undefined
      ? {}
      : { maxOutputTokens: request.maxOutputTokens }),
  };
  let content = "";
  let finishReason = "stop";
  let usage: Record<string, unknown> | undefined;
  const toolCalls: { id?: string; name: string; arguments?: unknown }[] = [];
  for await (const event of client.streamChat(input)) {
    switch (event.type) {
      case "run.message.delta": {
        const delta = (event.data as { delta?: string }).delta;
        if (typeof delta === "string") content += delta;
        break;
      }
      case "run.message.completed": {
        const data = event.data as {
          content?: string;
          finishReason?: string;
        };
        if (typeof data.content === "string") content = data.content;
        if (typeof data.finishReason === "string")
          finishReason = data.finishReason;
        break;
      }
      case "run.tool.requested": {
        const data = event.data as {
          toolCallId?: string;
          toolName: string;
          argumentsJson: string;
        };
        toolCalls.push({
          ...(data.toolCallId === undefined ? {} : { id: data.toolCallId }),
          name: data.toolName,
          arguments: JSON.parse(data.argumentsJson) as unknown,
        });
        finishReason = "tool_calls";
        break;
      }
      case "run.failed": {
        const data = event.data as { errorMessage?: string };
        throw new Error(data.errorMessage ?? "provider failed");
      }
      case "run.usage":
        usage = { ...event.data };
        break;
      case "run.cancelled":
        throw new Error("provider call cancelled");
      default:
        break;
    }
  }
  return {
    content,
    ...(toolCalls.length > 0 ? { toolCalls } : {}),
    finishReason,
    ...(usage === undefined ? {} : { usage }),
  };
}

export class CodingAgentApp {
  private service: NativeSessionService | null = null;
  private closing = false;
  /** Turns this process started or explicitly resumed: a `running` task held
   * by one of these is live local work that `recover()` must never disturb;
   * a `running` task that is NOT one of them belongs to a dead process, and
   * recreating the service is what adopts (or refuses to adopt) it honestly. */
  private readonly localTurns = new Set<string>();

  private constructor(
    readonly options: CodingAgentOptions,
    readonly store: NativeStore,
    service: NativeSessionService,
    private readonly profile: NativeResolvedAgentProfile,
  ) {
    this.service = service;
  }

  private live(): NativeSessionService {
    if (this.service === null)
      throw new Error("the coding agent app is closed");
    return this.service;
  }

  /**
   * Open (creating if needed) everything the agent needs. Nothing here starts a
   * model call: opening an app must not resume work by itself, or a crash loop
   * would become a model-call loop. The native service crash-reconciles on
   * construction and its worker never claims an interrupted turn.
   */
  static async open(options: CodingAgentOptions): Promise<CodingAgentApp> {
    if (!isAbsolute(options.repo))
      throw new Error(`repo must be an absolute path, got "${options.repo}"`);
    if (!isAbsolute(options.dbPath))
      throw new Error("dbPath must be an absolute path");
    mkdirSync(dirname(options.dbPath), { recursive: true });
    const profiles = await listNativeProfiles(options.profiles);
    const profile = profiles.find((item) => item.id === options.profileId);
    if (profile === undefined) {
      throw new Error(
        `Unknown profile "${options.profileId}". Known: ${profiles
          .map((p) => p.id)
          .join(", ")}.`,
      );
    }
    const store = await NativeStore.open(options.dbPath);
    try {
      const service = await CodingAgentApp.buildService({
        options,
        store,
        profile,
      });
      return new CodingAgentApp(options, store, service, profile);
    } catch (error) {
      await store.close();
      throw error;
    }
  }

  /** One service over the store: the tool lane (or the Rust provider lane,
   * when no provider was injected) plus the Rust coding-pack authority AND
   * the Rust-owned coding tools. Recreated by `recover()` — every
   * registered callback and descriptor is re-registered here, so a fresh
   * service is indistinguishable from the one `open()` built. */
  private static async buildService(app: {
    options: CodingAgentOptions;
    store: NativeStore;
    profile: NativeResolvedAgentProfile;
  }): Promise<NativeSessionService> {
    const { options, store, profile } = app;
    const wantedMode = options.mode ?? profile.permissionMode;
    if (NATIVE_MODES[wantedMode] !== true)
      throw new Error(
        `profile "${options.profileId}" has unsupported permissionMode "${wantedMode}"`,
      );
    const configuredMode: NativeMode = wantedMode as NativeMode;
    const provider = options.providerFactory
      ? options.providerFactory({
          baseUrl: options.baseUrl,
          defaultModel: options.model,
          ...(options.apiKeyEnv === undefined
            ? {}
            : {
                apiKey:
                  options.apiKeyEnv === undefined
                    ? undefined
                    : (process.env[options.apiKeyEnv] ?? undefined),
              }),
        })
      : undefined;
    let providerConfig:
      | {
          protocol: "chat-completions";
          providerId: string;
          connectionId: string;
          model: string;
        }
      | undefined;
    if (provider === undefined) {
      if (options.baseUrl === undefined)
        throw new Error(
          "without a providerFactory, baseUrl is required (the Rust provider lane calls it directly)",
        );
      const apiKey =
        options.apiKeyEnv === undefined
          ? null
          : (process.env[options.apiKeyEnv] ?? null);
      const connectionId =
        await NativeSessionService.registerProviderConnection(
          options.baseUrl,
          apiKey,
        );
      providerConfig = {
        protocol: "chat-completions",
        providerId: "local",
        connectionId,
        model: options.model,
      };
    }
    const service = await store.createService({
      ...(options.profiles === undefined ? {} : { profiles: options.profiles }),
      jsToolAuthority: {
        configuredMode,
        planToolNames: [SUBMIT_PLAN_TOOL_NAME],
        // A plan approval is the only way out of plan mode, and the HOST
        // decides what it grants. The model's requested mode is recorded,
        // never obeyed — and the granted mode can never be plan itself, so
        // a session cannot be escalated into the mode that blocks it.
        planGrantMode:
          options.mode === undefined || options.mode === "plan"
            ? "auto_edit"
            : options.mode,
      },
      ...(providerConfig === undefined ? {} : { providerConfig }),
    });
    if (options.contentAuthority)
      service.setContentAuthority(
        options.contentAuthority.authorize,
        options.contentAuthority.identity,
      );
    if (provider !== undefined) {
      const client = provider;
      service.setProvider((request) => finalResult(client, request));
    }
    // The coding tools are the Rust pack's: descriptors, bodies, guards,
    // sandbox, leases and standing approvals all execute in Rust on the
    // same executor lane, judged by the same durable mode this service's
    // authority reads. Registering them here (not in `open()`) is what
    // makes `recover()`'s fresh service indistinguishable from the first.
    await service.registerCodingTools({
      workspaceRoot: resolve(options.repo),
      sandbox: options.sandbox ?? "required",
      testCommands: [...(options.testCommands ?? ["bun test"])],
      configuredMode: configuredMode,
      planToolNames: [SUBMIT_PLAN_TOOL_NAME],
    });
    return service;
  }

  async ensureSession(): Promise<NativeSessionState> {
    const existing = await this.store.callStore<Record<string, unknown> | null>(
      "conversations.getChat",
      { chatId: this.options.sessionId },
    );
    if (existing !== null) {
      const state = await this.live().getState(this.options.sessionId);
      const effort = this.options.reasoningEffort;
      const descriptor = (state as unknown as NativeSessionState).descriptor;
      if (
        this.options.contentSelection !== undefined &&
        (!isDeepStrictEqual(
          descriptor.contentSelection?.template ?? null,
          this.options.contentSelection.template ?? null,
        ) ||
          !isDeepStrictEqual(
            descriptor.contentSelection?.skills ?? [],
            this.options.contentSelection.skills ?? [],
          ))
      )
        throw new Error("Use a new session ID to change contentSelection.");
      if (effort === undefined || descriptor.reasoningEffort === effort)
        return state as unknown as NativeSessionState;
      return (await this.live().setModel(this.options.sessionId, {
        model: descriptor.model ?? this.options.model,
        reasoningEffort: effort,
      })) as unknown as NativeSessionState;
    }
    const created = (await this.live().createSession({
      sessionId: this.options.sessionId,
      profileId: this.options.profileId,
      profileDigest: this.profile.digest,
      model: this.options.model,
      ...(this.options.reasoningEffort === undefined
        ? {}
        : { reasoningEffort: this.options.reasoningEffort }),
      ...(this.options.contentSelection === undefined
        ? {}
        : { contentSelection: this.options.contentSelection }),
      workspace: { root: resolve(this.options.repo), trusted: true },
      ...(this.options.mode === undefined
        ? {}
        : { executionMode: this.options.mode }),
    })) as unknown as NativeSessionState;
    return created;
  }

  /**
   * The native service starts its worker at construction and crash-reconciles
   * before that worker can claim anything; starting the app therefore means
   * giving a dead process's work one reconciliation pass.
   */
  async startWorker(): Promise<void> {
    await this.recover();
  }

  async stopWorker(): Promise<void> {
    // The worker lives and dies with the service; `close()` is the shutdown.
  }

  /**
   * Park work a previous process left behind as `interrupted`. The native
   * runtime reconciles only at service construction, so a reconciliation pass
   * here is a fresh service over the same store — the same code path a new
   * process runs, with the same policy (manual: park, never restart). A turn
   * this process itself started is live local work and is left alone; a
   * resumed turn never re-runs a tool whose outcome the log cannot decide
   * (durable receipts keep the uncertainty).
   */
  async recover(): Promise<void> {
    if (this.closing || this.service === null) return;
    const state = await this.state();
    const active = state.activeTurn;
    if (active === null || active.status !== "running") return;
    if (this.localTurns.has(active.turnId)) return; // live work of this process
    const previous = this.service;
    this.service = null;
    await previous.close();
    this.service = await CodingAgentApp.buildService({
      options: this.options,
      store: this.store,
      profile: this.profile,
    });
  }

  /**
   * An instruction. Typing while a turn runs queues it as a follow-up rather
   * than losing it, so every input needs an id — mint one when the caller has
   * no opinion, and the queue stays deduplicated and idempotent on retry.
   */
  async send(
    text: string,
    inputId?: string,
  ): Promise<
    { kind: "turn"; turnId: string } | { kind: "queued"; input: unknown }
  > {
    const result = await this.live().send(this.options.sessionId, {
      content: text,
      inputId: inputId ?? `input-${crypto.randomUUID()}`,
      whenBusy: "queue",
    });
    if (result.kind === "turn") this.localTurns.add(result.turnId);
    return result;
  }

  async followUp(text: string, inputId: string) {
    return this.live().followUp(this.options.sessionId, {
      inputId,
      content: text,
    });
  }

  async steer(text: string, inputId: string) {
    return this.live().steer(this.options.sessionId, {
      inputId,
      content: text,
    });
  }

  async state(): Promise<NativeSessionState> {
    return (await this.live().getState(
      this.options.sessionId,
    )) as unknown as NativeSessionState;
  }

  async status(turnId: string): Promise<TurnStatus | undefined> {
    const task = await this.store.callStore<{ status?: TurnStatus } | null>(
      "tasks.getTask",
      { taskId: turnId },
    );
    return task?.status;
  }

  /** The approval this session is parked on, if any. */
  async pendingApproval(): Promise<PendingApproval | undefined> {
    const state = await this.state();
    const request = state.waiting;
    if (request === null || request === undefined) return undefined;
    const content = request.content as Record<string, unknown> | undefined;
    const approval = state.waitingApproval ?? null;
    return {
      requestId: request.inputId,
      // Stable, so a retry of the same decision is the same answer, and a
      // DIFFERENT decision is refused rather than silently overriding a human.
      answerId: `ans:${request.inputId}`,
      toolName: approval?.toolName ?? "tool",
      ...(approval?.argsSha256 === undefined
        ? {}
        : { argsSha256: approval.argsSha256 }),
      prompt:
        typeof content?.prompt === "string"
          ? content.prompt
          : (approval?.prompt ?? ""),
      ...(state.waitingPlan === null || state.waitingPlan === undefined
        ? {}
        : {
            plan: {
              planDigest:
                state.waitingPlan.planDigest ??
                (typeof content?.argsSha256 === "string"
                  ? content.argsSha256
                  : ""),
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
      await this.live().approve(approval.requestId, approval.answerId, note);
    else await this.live().deny(approval.requestId, approval.answerId, note);
  }

  /** The durable execution mode the session is in, after any plan approval. */
  async executionMode(): Promise<string | undefined> {
    const execution = await this.live().getExecution(this.options.sessionId);
    return execution?.mode as string | undefined;
  }

  async setExecutionMode(mode: string): Promise<void> {
    await this.live().setExecutionMode(this.options.sessionId, mode);
  }

  /** Park a running turn `interrupted`; `resumeTurn` continues it. */
  async interrupt(turnId: string): Promise<void> {
    await this.live().interrupt(this.options.sessionId, turnId);
  }

  /** The session's interrupted turn, if it has one. */
  async interruptedTurn(): Promise<string | undefined> {
    const active = (await this.state()).activeTurn;
    return active?.status === "interrupted" ? active.turnId : undefined;
  }

  async resumeTurn(turnId: string): Promise<void> {
    await this.live().resume(this.options.sessionId, turnId);
    this.localTurns.add(turnId);
  }

  async turns(): Promise<AgentTurnSummary[]> {
    return (await this.live().listTurns(
      this.options.sessionId,
    )) as unknown as AgentTurnSummary[];
  }

  async usage(): Promise<AgentSessionUsage> {
    return (await this.live().getUsage(
      this.options.sessionId,
    )) as unknown as AgentSessionUsage;
  }

  async exportSession(): Promise<Record<string, unknown>> {
    return await this.live().exportSession(this.options.sessionId);
  }

  /** Takes effect on the NEXT turn; a turn already running keeps its request. */
  async setModel(
    model: string,
    reasoningEffort?: SessionReasoningEffort | null,
  ): Promise<NativeSessionState> {
    return (await this.live().setModel(this.options.sessionId, {
      model,
      ...(reasoningEffort === undefined ? {} : { reasoningEffort }),
    })) as unknown as NativeSessionState;
  }

  /** Every agent session in the database, newest first. */
  async listSessions(): Promise<SessionListing[]> {
    const listings: SessionListing[] = [];
    let before: string | undefined;
    for (;;) {
      const page = await this.store.callStore<
        {
          id: string;
          metadata?: Record<string, unknown>;
          updatedAt: number;
        }[]
      >("conversations.listChats", {
        opts: {
          limit: 100,
          includeArchived: true,
          ...(before === undefined ? {} : { before }),
        },
      });
      if (page.length === 0) break;
      for (const chat of page) {
        let descriptor: NativeSessionState["descriptor"];
        try {
          descriptor = (
            (await this.live().getState(
              chat.id,
            )) as unknown as NativeSessionState
          ).descriptor;
        } catch (error) {
          if (
            error instanceof NativeAgentError &&
            error.code === "session_not_found"
          )
            continue;
          throw error;
        }
        const turns = await this.turnsFor(chat.id);
        const last = turns[turns.length - 1];
        listings.push({
          sessionId: chat.id,
          profileId: descriptor.profile.id,
          ...(descriptor.model === undefined
            ? {}
            : { model: descriptor.model }),
          ...(descriptor.reasoningEffort === undefined
            ? {}
            : {
                reasoningEffort:
                  descriptor.reasoningEffort as SessionReasoningEffort,
              }),
          turns: turns.length,
          ...(last === undefined ? {} : { lastTurnStatus: last.status }),
        });
      }
      if (page.length < 100) break;
      before = String(page[page.length - 1]?.updatedAt);
    }
    return listings;
  }

  private async turnsFor(sessionId: string): Promise<AgentTurnSummary[]> {
    return (await this.live().listTurns(
      sessionId,
    )) as unknown as AgentTurnSummary[];
  }

  async cancel(turnId: string): Promise<void> {
    await this.live().cancel(this.options.sessionId, turnId);
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
    const messages = await this.store.callStore<
      {
        role: string;
        content: unknown;
        active?: boolean;
        metadata?: Record<string, unknown>;
      }[]
    >("conversations.listMessages", { chatId: this.options.sessionId });
    return messages
      .filter(
        (m) =>
          m.active !== false &&
          !(m.role === "assistant" && m.metadata?.["internal"] === true),
      )
      .map((m) => ({
        role: m.role,
        content: typeof m.content === "string" ? m.content : "",
        ...(typeof m.metadata?.["toolName"] === "string"
          ? { toolName: m.metadata["toolName"] as string }
          : {}),
      }));
  }

  /** Events of a turn from `afterSeq` on — the CLI's live activity feed. */
  async eventsAfter(
    turnId: string,
    afterSeq: number,
  ): Promise<{ events: AiRunEvent[]; nextSeq: number }> {
    const all = await this.store.callStore<
      { seq: number; event: AiRunEvent }[]
    >("tasks.listEvents", { taskId: turnId });
    const events = all.filter((e) => e.seq > afterSeq).map((e) => e.event);
    const last = all[all.length - 1];
    return {
      events,
      nextSeq: last === undefined ? afterSeq : Math.max(afterSeq, last.seq),
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
  ): Promise<TurnStatus> {
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
      if (TERMINAL[status] === true) return status;
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

  /**
   * Quitting: a running turn of THIS process is interrupted durably first —
   * the runtime parks it at a safe boundary (the reconciliation heartbeat
   * lands within ~10s) — and only then does the service close. A turn left
   * running by a dead process is never touched here; it stays `running`
   * until a later `recover()` parks it.
   */
  async close(): Promise<void> {
    if (this.closing) return;
    this.closing = true;
    const service = this.service;
    this.service = null;
    if (service !== null) {
      try {
        const active = (await service
          .getState(this.options.sessionId)
          .catch(() => null)) as NativeSessionState | null;
        const running =
          active?.activeTurn?.status === "running" ? active.activeTurn : null;
        if (running !== null && this.localTurns.has(running.turnId)) {
          await service.interrupt(this.options.sessionId, running.turnId);
          const until = Date.now() + 15_000;
          while (Date.now() < until) {
            const st = (await service
              .getState(this.options.sessionId)
              .catch(() => null)) as NativeSessionState | null;
            if (st?.activeTurn?.status !== "running") break;
            await new Promise((r) => setTimeout(r, 100));
          }
        }
      } catch {
        // Best-effort: the durable interrupt is recorded; the park needs no
        // live process, and the next open reconciles it.
      }
      await service.close();
    }
    // `close()` on the binding already settled the native coding lane:
    // live commands this process owns are taken down by the crate's own
    // teardown after the worker drained; anything a dead process left
    // behind is reconciled by the next boot's startup sweep, not here.
    await this.store.close();
  }
}
