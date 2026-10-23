import type { SecretStore } from "@agentkit/host";
import {
  auth,
  UnauthorizedError,
  type OAuthClientProvider,
  type OAuthDiscoveryState,
} from "@modelcontextprotocol/client";
import type {
  OAuthClientMetadata,
  StoredOAuthClientInformation,
  StoredOAuthTokens,
  OAuthTokens,
} from "@modelcontextprotocol/client";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import {
  defaultMcpTransportFactory,
  type McpTransportFactory,
} from "./transport.js";
import { McpError } from "./errors.js";

type OAuthFetch = (
  input: string | URL | Request,
  init?: RequestInit,
) => Promise<Response>;

export interface ManagedOAuthOptions {
  connectionId: string;
  accountId: string;
  serverUrl: string;
  redirectUrl: string;
  clientMetadata: OAuthClientMetadata;
  /** Pre-registered GitHub App/OAuth App information; stored only in SecretStore. */
  clientInformation?: StoredOAuthClientInformation;
  /** Application owns browser opening. */
  redirect(url: URL): Promise<void>;
  secrets: SecretStore;
  /** Explicitly trusted issuer origins, including resource origin. */
  allowedOrigins: readonly string[];
  fetch?: OAuthFetch;
  now?: () => number;
  revoke?(tokens: OAuthTokens): Promise<void>;
}

interface SavedTokens {
  tokens: StoredOAuthTokens;
  expiresAt: number;
}

function ordered(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(ordered);
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value)
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([key, item]) => [key, ordered(item)]),
    );
  return value;
}

function frozen<T>(value: T): T {
  if (value && typeof value === "object") {
    Object.values(value).forEach(frozen);
    Object.freeze(value);
  }
  return value;
}

function bindingRefusal(): never {
  throw new McpError(
    "mcp_auth_required",
    "Stored OAuth credentials require explicit reauthorization.",
  );
}

/** Account-scoped PKCE, state, credentials and single-flight refresh using SDK discovery.
 * Stored material is bound to the canonical resource and configured OAuth identity.
 * Legacy/unmatched entries require invalidateCredentials("all") and fresh authorization.
 */
export class ManagedOAuthProvider implements OAuthClientProvider {
  readonly redirectUrl: string;
  readonly clientMetadata: OAuthClientMetadata;
  private readonly prefix: string;
  private readonly options: ManagedOAuthOptions;
  private readonly binding: Promise<string>;
  private queue: Promise<unknown> = Promise.resolve();
  private readonly now: () => number;
  private revoked = false;
  constructor(options: ManagedOAuthOptions) {
    this.options = {
      ...options,
      serverUrl: new URL(options.serverUrl).href,
      redirectUrl: options.redirectUrl,
      clientMetadata: structuredClone(options.clientMetadata),
      clientInformation: structuredClone(options.clientInformation),
      allowedOrigins: [...options.allowedOrigins],
    };
    this.redirectUrl = this.options.redirectUrl;
    this.clientMetadata = frozen(structuredClone(this.options.clientMetadata));
    this.prefix = `oauth/${encodeURIComponent(options.connectionId)}/${encodeURIComponent(options.accountId)}/`;
    this.now = options.now ?? Date.now;
    if (this.checkUrl(this.options.serverUrl).hash)
      throw new McpError(
        "mcp_invalid_config",
        "OAuth resource URI must not contain a fragment.",
      );
    const redirect = new URL(this.redirectUrl);
    if (redirect.hash || redirect.search)
      throw new McpError(
        "mcp_invalid_config",
        "OAuth redirect URI must not contain query or fragment.",
      );
    const identity = ordered({
      resource: this.options.serverUrl,
      redirect: this.redirectUrl,
      clientMetadata: this.options.clientMetadata,
      clientInformation: this.options.clientInformation,
      allowedOrigins: [...this.options.allowedOrigins].sort(),
    });
    this.binding = crypto.subtle
      .digest("SHA-256", new TextEncoder().encode(JSON.stringify(identity)))
      .then((digest) =>
        Array.from(new Uint8Array(digest), (byte) =>
          byte.toString(16).padStart(2, "0"),
        ).join(""),
      );
  }
  private exclusive<T>(fn: () => Promise<T>): Promise<T> {
    const next = this.queue.then(fn);
    this.queue = next.catch(() => {});
    return next;
  }
  private checkUrl(input: string | URL) {
    const url = new URL(input);
    if (
      url.protocol !== "https:" ||
      url.username ||
      url.password ||
      !this.options.allowedOrigins.includes(url.origin)
    )
      throw new McpError(
        "mcp_invalid_config",
        "OAuth endpoint is outside trusted HTTPS origins.",
      );
    return url;
  }
  private async read<T>(key: string): Promise<T | undefined> {
    const value = await this.options.secrets.get(this.prefix + key);
    if (value === null) return undefined;
    let saved: { version?: number; binding?: string; value?: T };
    try {
      saved = JSON.parse(value);
    } catch {
      bindingRefusal();
    }
    if (
      !saved ||
      saved.version !== 1 ||
      saved.binding !== (await this.binding) ||
      !Object.hasOwn(saved, "value")
    )
      bindingRefusal();
    return saved.value;
  }
  private async write(key: string, value: unknown) {
    await this.options.secrets.set(
      this.prefix + key,
      JSON.stringify({
        version: 1,
        binding: await this.binding,
        value,
      }),
    );
  }
  async state() {
    const state = crypto.randomUUID() + crypto.randomUUID();
    await this.write("state", {
      value: state,
      expiresAt: this.now() + 600_000,
    });
    return state;
  }
  async clientInformation() {
    return structuredClone(
      (await this.read<StoredOAuthClientInformation>("client")) ??
        this.options.clientInformation,
    );
  }
  async saveClientInformation(value: StoredOAuthClientInformation) {
    const expected = this.options.clientInformation;
    if (
      expected &&
      (expected.client_id !== value.client_id ||
        (expected.issuer && expected.issuer !== value.issuer))
    )
      bindingRefusal();
    await this.write("client", value);
  }
  private async storedTokens() {
    const saved = await this.read<SavedTokens>("tokens");
    const client = await this.clientInformation();
    if (saved && client?.issuer && saved.tokens.issuer !== client.issuer)
      bindingRefusal();
    return saved;
  }
  async tokens() {
    return (await this.storedTokens())?.tokens;
  }
  async saveTokens(tokens: StoredOAuthTokens) {
    if (this.revoked)
      throw new McpError("mcp_auth_required", "OAuth connection is revoked.");
    const client = await this.clientInformation();
    if (client?.issuer && tokens.issuer !== client.issuer) bindingRefusal();
    const previous = await this.tokens();
    const next = {
      ...tokens,
      ...(tokens.refresh_token
        ? {}
        : previous?.issuer === tokens.issuer && previous?.refresh_token
          ? { refresh_token: previous.refresh_token }
          : {}),
    };
    await this.write("tokens", {
      tokens: next,
      expiresAt: this.now() + (tokens.expires_in ?? 3600) * 1000,
    });
  }
  async redirectToAuthorization(url: URL) {
    this.checkUrl(url);
    await this.options.redirect(url);
  }
  async saveCodeVerifier(value: string) {
    await this.write("verifier", value);
  }
  async codeVerifier() {
    const value = await this.read<string>("verifier");
    if (!value)
      throw new McpError("mcp_auth_required", "Missing PKCE verifier.");
    return value;
  }
  async validateResourceURL(serverUrl: string | URL, resource?: string) {
    const expected = new URL(this.options.serverUrl);
    if (new URL(serverUrl).href !== expected.href)
      throw new McpError("mcp_auth_required", "OAuth server identity changed.");
    const target = resource ? this.checkUrl(resource) : expected;
    if (target.href !== expected.href)
      throw new McpError(
        "mcp_auth_required",
        "OAuth resource identity mismatch.",
      );
    return target;
  }
  async saveDiscoveryState(state: OAuthDiscoveryState) {
    if (state.authorizationServerUrl)
      this.checkUrl(state.authorizationServerUrl);
    await this.write("discovery", state);
  }
  discoveryState() {
    return this.read<OAuthDiscoveryState>("discovery");
  }
  async invalidateCredentials(
    scope: "all" | "client" | "tokens" | "verifier" | "discovery",
  ) {
    const keys =
      scope === "all"
        ? ["client", "tokens", "verifier", "discovery", "state"]
        : [scope];
    for (const key of keys)
      await this.options.secrets.delete(this.prefix + key);
  }
  private readonly checkedFetch: OAuthFetch = async (input, init) => {
    const url =
      typeof input === "string" || input instanceof URL ? input : input.url;
    this.checkUrl(url);
    const response = await (this.options.fetch ?? fetch)(input, {
      ...init,
      redirect: "error",
    });
    return response;
  };
  /** SDK performs metadata discovery, issuer checks, PKCE and token exchange. */
  authorize(): Promise<void> {
    return this.exclusive(async () => {
      if (this.revoked)
        throw new McpError("mcp_auth_required", "OAuth connection is revoked.");
      const saved = await this.storedTokens();
      if (saved && saved.expiresAt > this.now() + 30_000) return;
      const result = await auth(this, {
        serverUrl: this.options.serverUrl,
        fetchFn: this.checkedFetch,
      });
      if (result !== "AUTHORIZED")
        throw new McpError(
          "mcp_auth_required",
          "Complete OAuth authorization in the application.",
        );
    });
  }
  complete(callbackUrl: string): Promise<void> {
    return this.exclusive(async () => {
      const callback = new URL(callbackUrl);
      const expected = new URL(this.redirectUrl);
      const state = await this.read<{ value: string; expiresAt: number }>(
        "state",
      );
      if (
        callback.origin !== expected.origin ||
        callback.pathname !== expected.pathname ||
        callback.hash ||
        !state ||
        state.expiresAt < this.now() ||
        callback.searchParams.get("state") !== state.value ||
        !callback.searchParams.get("code") ||
        callback.searchParams.has("error")
      )
        throw new McpError(
          "mcp_auth_required",
          "Invalid or expired OAuth callback.",
        );
      await this.options.secrets.delete(`${this.prefix}state`);
      try {
        if (
          (await auth(this, {
            serverUrl: this.options.serverUrl,
            authorizationCode: callback.searchParams.get("code")!,
            ...(callback.searchParams.has("iss")
              ? { iss: callback.searchParams.get("iss")! }
              : {}),
            fetchFn: this.checkedFetch,
          })) !== "AUTHORIZED"
        )
          throw new McpError(
            "mcp_auth_required",
            "OAuth authorization is incomplete.",
          );
      } finally {
        await this.options.secrets.delete(`${this.prefix}verifier`);
      }
    });
  }
  async disconnect() {
    return this.exclusive(async () => {
      this.revoked = true;
      try {
        let tokens: StoredOAuthTokens | undefined;
        try {
          tokens = await this.tokens();
        } catch (error) {
          if (
            !(error instanceof McpError) ||
            error.code !== "mcp_auth_required"
          )
            throw error;
        }
        if (tokens) await this.options.revoke?.(tokens);
      } finally {
        await this.invalidateCredentials("all");
      }
    });
  }
  /** No resource replay on 401: a subsequent explicit reconnect coordinates refresh. */
  readonly fetch: OAuthFetch = async (input, init) => {
    const target = new URL(
      typeof input === "string" || input instanceof URL ? input : input.url,
    );
    const expected = new URL(this.options.serverUrl);
    if (target.href !== expected.href)
      throw new McpError(
        "mcp_auth_required",
        "OAuth transport resource changed.",
      );
    await this.authorize();
    const tokens = await this.tokens();
    const headers = new Headers(init?.headers);
    headers.set("Authorization", `Bearer ${tokens!.access_token}`);
    if (this.revoked)
      throw new McpError("mcp_auth_required", "OAuth connection is revoked.");
    const response = await this.checkedFetch(input, { ...init, headers });
    if (response.status === 403)
      throw new McpError(
        "mcp_invocation_refused",
        "The remote account lacks permission for this operation.",
      );
    if (response.status === 401) {
      const saved = await this.storedTokens();
      if (saved) await this.write("tokens", { ...saved, expiresAt: 0 });
      throw new UnauthorizedError("OAuth authorization required.");
    }
    return response;
  };
}

/** Resolve provider by immutable connection alias; never use a global current account. */
export function oauthTransportFactory(
  providerForAlias: (alias: string) => ManagedOAuthProvider | undefined,
): McpTransportFactory {
  return async (request) => {
    const provider = providerForAlias(request.alias);
    if (!provider || request.transport.kind !== "http")
      return defaultMcpTransportFactory(request);
    await provider.authorize();
    return new StreamableHTTPClientTransport(new URL(request.transport.url), {
      fetch: provider.fetch,
      requestInit: { headers: request.transport.headers },
    });
  };
}
