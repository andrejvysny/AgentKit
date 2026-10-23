import type { SecretStore } from "@agentkit/host";
import {
  auth,
  UnauthorizedError,
  type OAuthClientProvider,
  type OAuthDiscoveryState,
} from "@modelcontextprotocol/sdk/client/auth.js";
import type {
  OAuthClientMetadata,
  OAuthClientInformationMixed,
  OAuthTokens,
} from "@modelcontextprotocol/sdk/shared/auth.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
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
  clientInformation?: OAuthClientInformationMixed;
  /** Application owns browser opening. */
  redirect(url: URL): Promise<void>;
  secrets: SecretStore;
  /** Explicitly trusted issuer origins, including resource origin. */
  allowedOrigins: readonly string[];
  fetch?: OAuthFetch;
  now?: () => number;
  revoke?(tokens: OAuthTokens): Promise<void>;
}

/** Account-scoped PKCE, state, credentials and single-flight refresh using SDK discovery. */
export class ManagedOAuthProvider implements OAuthClientProvider {
  readonly redirectUrl: string;
  readonly clientMetadata: OAuthClientMetadata;
  private readonly prefix: string;
  private queue: Promise<unknown> = Promise.resolve();
  private readonly now: () => number;
  private revoked = false;
  constructor(private readonly options: ManagedOAuthOptions) {
    this.redirectUrl = options.redirectUrl;
    this.clientMetadata = structuredClone(options.clientMetadata);
    this.prefix = `oauth/${encodeURIComponent(options.connectionId)}/${encodeURIComponent(options.accountId)}/`;
    this.now = options.now ?? Date.now;
    this.checkUrl(options.serverUrl);
    const redirect = new URL(options.redirectUrl);
    if (redirect.hash || redirect.search)
      throw new McpError(
        "mcp_invalid_config",
        "OAuth redirect URI must not contain query or fragment.",
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
    return value === null ? undefined : (JSON.parse(value) as T);
  }
  private async write(key: string, value: unknown) {
    await this.options.secrets.set(this.prefix + key, JSON.stringify(value));
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
    return (
      (await this.read<OAuthClientInformationMixed>("client")) ??
      this.options.clientInformation
    );
  }
  async saveClientInformation(value: OAuthClientInformationMixed) {
    await this.write("client", value);
  }
  async tokens() {
    return (await this.read<{ tokens: OAuthTokens }>("tokens"))?.tokens;
  }
  async saveTokens(tokens: OAuthTokens) {
    if (this.revoked)
      throw new McpError("mcp_auth_required", "OAuth connection is revoked.");
    const previous = await this.tokens();
    const next = {
      ...tokens,
      ...(tokens.refresh_token
        ? {}
        : previous?.refresh_token
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
    if (
      target.origin !== expected.origin ||
      target.pathname !== expected.pathname
    )
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
      const saved = await this.read<{ tokens: OAuthTokens; expiresAt: number }>(
        "tokens",
      );
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
      await this.options.secrets.delete(this.prefix + "state");
      try {
        if (
          (await auth(this, {
            serverUrl: this.options.serverUrl,
            authorizationCode: callback.searchParams.get("code")!,
            fetchFn: this.checkedFetch,
          })) !== "AUTHORIZED"
        )
          throw new McpError(
            "mcp_auth_required",
            "OAuth authorization is incomplete.",
          );
      } finally {
        await this.options.secrets.delete(this.prefix + "verifier");
      }
    });
  }
  async disconnect() {
    return this.exclusive(async () => {
      this.revoked = true;
      const tokens = await this.tokens();
      try {
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
    if (
      target.origin !== expected.origin ||
      target.pathname !== expected.pathname
    )
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
      const saved = await this.read<{ tokens: OAuthTokens; expiresAt: number }>(
        "tokens",
      );
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
