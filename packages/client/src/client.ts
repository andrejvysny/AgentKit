/**
 * The client: one method per operation in `REST_ROUTES`, typed with the
 * contract's own DTOs.
 *
 * EXHAUSTIVE BY CONSTRUCTION. The method table below is
 * `satisfies Record<RestOperation, unknown>`, so a route added to
 * `@agentkit/contracts` breaks this file's compile until it is implemented, and
 * a method named after a route that no longer exists fails the excess-property
 * check. That is the same guarantee `@agentkit/transport-http`'s dispatch table
 * gives on the server side, for the same reason: a contract route reachable from
 * neither end is discoverable only as somebody's 404.
 *
 * ARGUMENT SHAPE. `(params, body?, opts?)`, with `params` OMITTED on the routes
 * whose path and query define none — `createChat(body)`, `getSettings()`. A
 * mandatory `undefined` first argument on a third of the surface buys uniformity
 * nobody reads and costs every call site a hole to fill.
 *
 * WHAT THIS CLIENT DOES NOT DO: it does not validate responses against the
 * contract's JSON Schemas. The DTO types describe what a conforming server
 * sends; checking it would drag TypeBox and Ajv into every browser bundle to
 * re-litigate what the server already validated on the way out. A client that
 * needs to defend against a non-conforming server can compile the schemas from
 * `@agentkit/contracts` itself.
 */
import type {
  AiRunEvent,
  ApplyProposalRequest,
  ChatDto,
  CreateChatRequest,
  CreateMcpServerRequest,
  CreateProviderRequest,
  ForkChatRequest,
  GrantAllowanceRequest,
  McpServerDto,
  MessageDto,
  MessagePageDto,
  MessageSearchResponse,
  ModelDto,
  ProposalDecisionRequest,
  ProposalDto,
  ProposalStatusDto,
  ProviderDto,
  RegenerateMessageRequest,
  RestOperation,
  RunDto,
  SettingsDto,
  SubmitMessageRequest,
  SubmitMessageResponse,
  TestProviderResponse,
  ToolDefinitionDto,
  ToolEventDto,
  UpdateChatRequest,
  UpdateMcpServerRequest,
  UpdateProviderRequest,
  UpdateSettingsRequest,
  VersionDto,
  WriteAllowanceDto,
  WriteAllowanceListResponse,
} from "@agentkit/contracts";
import { drainRun, streamRun, type StreamRunOptions } from "./stream.js";
import {
  createTransport,
  newIdempotencyKey,
  type AgentKitClientOptions,
  type IdempotentRequestOptions,
  type IdempotentResult,
  type RequestOptions,
} from "./transport.js";

export function createAgentKitClient(options: AgentKitClientOptions) {
  const transport = createTransport(options);

  const rest = {
    // -----------------------------------------------------------------------
    // Chats
    // -----------------------------------------------------------------------

    /** Create a conversation. The server mints the id. */
    createChat: (body: CreateChatRequest = {}, opts?: RequestOptions) =>
      transport.json<ChatDto>("createChat", { body, options: opts }),

    /** Newest first; archived chats are hidden. `before` pages backwards. */
    listChats: (
      params: { limit?: number; before?: string } = {},
      opts?: RequestOptions,
    ) =>
      transport.json<ChatDto[]>("listChats", { query: params, options: opts }),

    getChat: (params: { chatId: string }, opts?: RequestOptions) =>
      transport.json<ChatDto>("getChat", { path: params, options: opts }),

    /** Rename, re-tag, archive. `metadata` REPLACES the stored bag. */
    updateChat: (
      params: { chatId: string },
      body: UpdateChatRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<ChatDto>("updateChat", {
        path: params,
        body,
        options: opts,
      }),

    /** The chat and everything the host holds about it. 409 while a run is live. */
    deleteChat: (params: { chatId: string }, opts?: RequestOptions) =>
      transport.empty("deleteChat", { path: params, options: opts }),

    /** One page of the chat's ACTIVE PATH, oldest first. */
    listMessages: (
      params: { chatId: string; limit?: number; cursor?: string },
      opts?: RequestOptions,
    ) =>
      transport.json<MessagePageDto>("listMessages", {
        path: { chatId: params.chatId },
        query: { limit: params.limit, cursor: params.cursor },
        options: opts,
      }),

    /**
     * Submit a turn, and hand back the `Idempotency-Key` that did it.
     *
     * The key is REQUIRED by the route and minted here when the caller supplies
     * none — but it is returned either way, because that is the only thing that
     * makes the retry of a timed-out submit safe. A caller that retries with the
     * same key lands on the SAME run and gets the same message ids back; one
     * that retries without it asks the same question twice.
     */
    submitMessage: async (
      params: { chatId: string },
      body: SubmitMessageRequest,
      opts?: IdempotentRequestOptions,
    ): Promise<IdempotentResult<SubmitMessageResponse>> => {
      const idempotencyKey = opts?.idempotencyKey ?? newIdempotencyKey();
      const result = await transport.json<SubmitMessageResponse>(
        "submitMessage",
        { path: params, body, idempotencyKey, options: opts },
      );
      return { result, idempotencyKey };
    },

    /**
     * Answer the same question again, as a new branch beside the answer it
     * already has. Keyed exactly like {@link submitMessage}, on its own
     * derivation — a submit and a regenerate under one key cannot collide.
     */
    regenerateMessage: async (
      params: { chatId: string; messageId: string },
      body: RegenerateMessageRequest = {},
      opts?: IdempotentRequestOptions,
    ): Promise<IdempotentResult<SubmitMessageResponse>> => {
      const idempotencyKey = opts?.idempotencyKey ?? newIdempotencyKey();
      const result = await transport.json<SubmitMessageResponse>(
        "regenerateMessage",
        { path: params, body, idempotencyKey, options: opts },
      );
      return { result, idempotencyKey };
    },

    /** Copy the active path up to a message into a NEW chat. */
    forkChat: (
      params: { chatId: string },
      body: ForkChatRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<ChatDto>("forkChat", {
        path: params,
        body,
        options: opts,
      }),

    /** Full-text search over message bodies; `q` is required, blank is legal. */
    searchMessages: (
      params: { q: string; chatId?: string; limit?: number },
      opts?: RequestOptions,
    ) =>
      transport.json<MessageSearchResponse>("searchMessages", {
        query: params,
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // Branching
    // -----------------------------------------------------------------------

    /** Make this message's branch active; answers with the path that became active. */
    activateBranch: (params: { messageId: string }, opts?: RequestOptions) =>
      transport.json<MessagePageDto>("activateBranch", {
        path: params,
        options: opts,
      }),

    /** Siblings sharing a parent, INCLUDING this message, `branchIndex` ascending. */
    listSiblings: (params: { messageId: string }, opts?: RequestOptions) =>
      transport.json<MessageDto[]>("listSiblings", {
        path: params,
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // Runs
    // -----------------------------------------------------------------------

    getRun: (params: { runId: string }, opts?: RequestOptions) =>
      transport.json<RunDto>("getRun", { path: params, options: opts }),

    /**
     * The run's events, resuming across dropped connections. See `stream.ts`.
     *
     * Takes a bare `runId` rather than a params object because it is the one
     * operation whose second argument is not a body — the shape that reads as
     * `for await (const event of client.streamRun(runId))` is the shape callers
     * actually write.
     */
    streamRun: (runId: string, opts?: StreamRunOptions) =>
      streamRun({ transport, runId }, opts),

    /** 202: a queued run settles at once, a running one is asked to stop. */
    cancelRun: (params: { runId: string }, opts?: RequestOptions) =>
      transport.json<RunDto>("cancelRun", { path: params, options: opts }),

    /** A chat's tool history, projected out of its runs' event logs. */
    listToolEvents: (
      params: { chatId: string; limit?: number },
      opts?: RequestOptions,
    ) =>
      transport.json<ToolEventDto[]>("listToolEvents", {
        path: { chatId: params.chatId },
        query: { limit: params.limit },
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // Proposals
    // -----------------------------------------------------------------------

    listProposals: (
      params: { chatId: string; limit?: number; status?: ProposalStatusDto },
      opts?: RequestOptions,
    ) =>
      transport.json<ProposalDto[]>("listProposals", {
        path: { chatId: params.chatId },
        query: { limit: params.limit, status: params.status },
        options: opts,
      }),

    approveProposal: (
      params: { proposalId: string },
      body: ProposalDecisionRequest = {},
      opts?: RequestOptions,
    ) =>
      transport.json<ProposalDto>("approveProposal", {
        path: params,
        body,
        options: opts,
      }),

    rejectProposal: (
      params: { proposalId: string },
      body: ProposalDecisionRequest = {},
      opts?: RequestOptions,
    ) =>
      transport.json<ProposalDto>("rejectProposal", {
        path: params,
        body,
        options: opts,
      }),

    /** `operationId` is the CLIENT's idempotency key for the side effect. */
    applyProposal: (
      params: { proposalId: string },
      body: ApplyProposalRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<ProposalDto>("applyProposal", {
        path: params,
        body,
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // Providers and models
    // -----------------------------------------------------------------------

    listProviders: (opts?: RequestOptions) =>
      transport.json<ProviderDto[]>("listProviders", { options: opts }),

    /** `apiKey` is WRITE-ONLY: it goes in and never comes back out. */
    createProvider: (body: CreateProviderRequest, opts?: RequestOptions) =>
      transport.json<ProviderDto>("createProvider", { body, options: opts }),

    updateProvider: (
      params: { providerId: string },
      body: UpdateProviderRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<ProviderDto>("updateProvider", {
        path: params,
        body,
        options: opts,
      }),

    deleteProvider: (params: { providerId: string }, opts?: RequestOptions) =>
      transport.empty("deleteProvider", { path: params, options: opts }),

    listModels: (params: { providerId: string }, opts?: RequestOptions) =>
      transport.json<ModelDto[]>("listModels", {
        path: params,
        options: opts,
      }),

    /** Re-probe the provider's catalogue and replace it. A write, not a read. */
    refreshProviderModels: (
      params: { providerId: string },
      opts?: RequestOptions,
    ) =>
      transport.json<ModelDto[]>("refreshProviderModels", {
        path: params,
        options: opts,
      }),

    /** `ok: false` arrives as a 200 — the probe ran and reported a failure. */
    testProvider: (params: { providerId: string }, opts?: RequestOptions) =>
      transport.json<TestProviderResponse>("testProvider", {
        path: params,
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // Settings and write policy
    // -----------------------------------------------------------------------

    getSettings: (opts?: RequestOptions) =>
      transport.json<SettingsDto>("getSettings", { options: opts }),

    updateSettings: (body: UpdateSettingsRequest, opts?: RequestOptions) =>
      transport.json<SettingsDto>("updateSettings", { body, options: opts }),

    /** An allowance belongs to exactly one conversation, and names it in the path. */
    listAllowances: (params: { chatId: string }, opts?: RequestOptions) =>
      transport.json<WriteAllowanceListResponse>("listAllowances", {
        path: params,
        options: opts,
      }),

    /** The chat is the path; the body says only what is being granted. */
    grantAllowance: (
      params: { chatId: string },
      body: GrantAllowanceRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<WriteAllowanceDto>("grantAllowance", {
        path: params,
        body,
        options: opts,
      }),

    /** A grant is revoked by the chat that owns it — both ids are in the path. */
    revokeAllowance: (
      params: { chatId: string; allowanceId: string },
      opts?: RequestOptions,
    ) =>
      transport.empty("revokeAllowance", {
        path: params,
        options: opts,
      }),

    // -----------------------------------------------------------------------
    // MCP servers, tools, version
    // -----------------------------------------------------------------------

    listMcpServers: (opts?: RequestOptions) =>
      transport.json<McpServerDto[]>("listMcpServers", { options: opts }),

    /** `alias` is the tool namespace and must be unique — 409 `duplicate_alias`. */
    createMcpServer: (body: CreateMcpServerRequest, opts?: RequestOptions) =>
      transport.json<McpServerDto>("createMcpServer", { body, options: opts }),

    updateMcpServer: (
      params: { serverId: string },
      body: UpdateMcpServerRequest,
      opts?: RequestOptions,
    ) =>
      transport.json<McpServerDto>("updateMcpServer", {
        path: params,
        body,
        options: opts,
      }),

    deleteMcpServer: (params: { serverId: string }, opts?: RequestOptions) =>
      transport.empty("deleteMcpServer", { path: params, options: opts }),

    /** The chat-independent tool catalogue; 501 where a host has none. */
    listTools: (opts?: RequestOptions) =>
      transport.json<ToolDefinitionDto[]>("listTools", { options: opts }),

    /** Contract version and REST surface version — they move independently. */
    getVersion: (opts?: RequestOptions) =>
      transport.json<VersionDto>("getVersion", { options: opts }),
    // The exhaustiveness gate. Both directions: a missing operation is a
    // missing property, and an operation this table invented is an excess one.
  } satisfies Record<RestOperation, unknown>;

  return {
    ...rest,

    /**
     * ONE resumed pass over the log past `lastEventId`, for the events a live
     * stream cannot see: the host appends `run.verification` AFTER the terminal
     * event, and the stream closed at the terminal event.
     *
     * Not a REST operation — the same `streamRun` route, read once instead of
     * followed, which is why it sits outside the exhaustive table above.
     */
    drainRun: (
      runId: string,
      lastEventId?: string,
      opts?: RequestOptions,
    ): Promise<AiRunEvent[]> =>
      drainRun({ transport, runId }, lastEventId, opts),

    /** The resolved base URL, trailing slash stripped. */
    baseUrl: transport.baseUrl,
  };
}

/** Everything {@link createAgentKitClient} returns. */
export type AgentKitClient = ReturnType<typeof createAgentKitClient>;
