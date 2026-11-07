/** One model request: the JS provider returns the final assistant result. */
export interface ProviderRequest {
  runId: string;
  model: string;
  messages: unknown[];
  tools: unknown[];
  temperature?: number | null;
  maxOutputTokens?: number | null;
  /** Request-local managed content, outside the durable transcript. */
  runContent?: { messages: readonly unknown[] };
  /**
   * The session descriptor's reasoning effort, carried verbatim from the
   * durable turn (`rust/crates/napi/src/provider.rs`). Absent means the run
   * has none — the key is omitted rather than sent as `null`, exactly as the
   * TypeScript reference request builder omits an undefined effort
   * (`packages/core/src/providers/responses-request.ts`). Adapters that
   * support effort should pass it through; ones that do not simply ignore
   * it, since an effort is not a sampling parameter an account-backed
   * adapter rejects.
   */
  reasoningEffort?: "minimal" | "low" | "medium" | "high" | null;
  /**
   * Aborts when the host has given up on this callback — run cancellation
   * (`cancel` notification) or `close()` (`discard`/drain). The provider is
   * expected to thread it into its fetch; the bridge settles the call as
   * cancelled regardless, so honoring the signal is what stops the network,
   * not what stops the run.
   */
  signal?: AbortSignal;
  /** Call after awaited preparation, immediately before a custom provider send. */
  checkAuthority?: () => Promise<void>;
}

export interface ProviderToolCall {
  id?: string;
  name: string;
  arguments?: unknown;
}

export interface ProviderResult {
  content?: string;
  toolCalls?: ProviderToolCall[];
  finishReason?: string;
  usage?: Record<string, unknown> | null;
}

export type ProviderFunction = (
  request: ProviderRequest,
) => Promise<ProviderResult> | ProviderResult;

/** One tool execution: the JS tool returns the host tool-result shape. */
export interface ToolCallRequest {
  name: string;
  toolCallId: string | null;
  attemptId: string | null;
  runId: string;
  input: unknown;
  /**
   * Aborts when the host has given up on this invocation: run cancellation
   * delivers the bridge `cancel` notification, and `close()` aborts
   * everything still in flight. Consumers that own side effects (the coding
   * pack spawns process groups) MUST thread this into what they start; a
   * tool that ignores it keeps effects running after cancellation, and the
   * runtime has already reported the call as settled.
   */
  signal?: AbortSignal;
}

export interface ToolResultJson {
  ok?: boolean;
  data?: unknown;
  sources?: unknown[];
  warnings?: string[];
  truncated?: boolean;
  limits?: { profile: string; maxBytes: number; maxItems?: number };
  modelData?: unknown;
  summary?: string;
  status?: string;
}

export interface ToolDefinitionJson {
  name: string;
  version?: string;
  effect?: "read" | "write";
  capability?: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  timeoutMs?: number;
}

export type ToolRunFunction = (
  input: unknown,
  request: ToolCallRequest,
) => Promise<ToolResultJson | unknown> | ToolResultJson | unknown;

/** MCP form/URL elicitation. The host owns any browser interaction. */
export interface McpElicitationRequest {
  alias: string;
  params: unknown;
  signal: AbortSignal;
}
export interface McpElicitationResult {
  action: "accept" | "decline" | "cancel";
  content?: Record<string, unknown>;
}
export type McpElicitationHandler = (
  request: McpElicitationRequest,
) => McpElicitationResult | Promise<McpElicitationResult>;

/** Durable credentials remain in the host vault; references alone may persist in config. */
export interface ProviderSecretStore {
  get(
    reference: string,
    signal: AbortSignal,
  ): string | null | Promise<string | null>;
  put(
    reference: string,
    value: string,
    signal: AbortSignal,
  ): void | Promise<void>;
  delete(reference: string, signal: AbortSignal): void | Promise<void>;
}
export interface ProviderConsentRequest {
  actorId: string;
  connectionId: string;
  generation: number;
  account: string;
  providerKind: string;
  destination: string;
  capability: string;
  model?: string | null;
  /** Exact serialized outgoing body approved by this request; empty for discovery. */
  payloadJson: string;
  disclosedData: readonly string[];
  signal: AbortSignal;
}
export type ProviderConsentHandler = (
  request: ProviderConsentRequest,
) => { approved: boolean } | Promise<{ approved: boolean }>;

/** OAuth credentials and browser interaction are owned by the embedding host. */
export interface McpOAuthHttpRequest {
  /** Host HTTP implementations must disable redirects. */
  redirect: "error";
  url: string;
  method: string;
  headers: Record<string, string>;
  body: string;
  signal: AbortSignal;
}
export interface McpOAuthHttpResponse {
  status: number;
  headers?: Record<string, string>;
  body: string;
}
export interface McpOAuthCallbacks {
  /** Trusted host HTTP port; enabled only by explicit OAuth options. */
  http?(
    request: McpOAuthHttpRequest,
  ): McpOAuthHttpResponse | Promise<McpOAuthHttpResponse>;
  secrets: {
    get(
      key: string,
      signal: AbortSignal,
    ): string | null | Promise<string | null>;
    set(key: string, value: string, signal: AbortSignal): void | Promise<void>;
    delete(key: string, signal: AbortSignal): void | Promise<void>;
  };
  redirect(request: {
    alias: string;
    url: string;
    signal: AbortSignal;
  }): void | Promise<void>;
}
