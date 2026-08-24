import type { SecretStore } from "@agentkit/host";
import type { McpTransportConfig } from "./config.js";
import { McpError } from "./errors.js";

/**
 * Resolved secret material for ONE server, plus the redactor that keeps it out
 * of everything else.
 *
 * Two operations, deliberately paired in one object: whatever value we
 * substitute INTO a transport config, we must be able to substitute back OUT of
 * any string derived from it. A connect failure carries a URL; a spawn failure
 * carries a command line; a debug log carries headers. Each of those is
 * config-derived text, and after substitution each may contain a live token.
 */
export interface McpSecretMaterial {
  /** `${placeholder}` token → resolved value. Never logged, never serialized. */
  readonly placeholders: ReadonlySet<string>;
  /** Replace every `${placeholder}` this material knows with its value. */
  substitute(value: string): string;
  /** Replace every resolved VALUE with `***`. Apply to anything user-visible. */
  redact(text: string): string;
}

/** Material for a server that declares no secrets: substitution and redaction are no-ops. */
export const EMPTY_SECRET_MATERIAL: McpSecretMaterial = {
  placeholders: new Set<string>(),
  substitute: (value) => value,
  redact: (text) => text,
};

/**
 * Resolve `secretRefs` against the store.
 *
 * A null from the store is an error here, not a normal state: the host wrote the
 * ref into this config on purpose, so an absent value means the server cannot be
 * built as described. The error names the placeholder and the ref — both are
 * safe, both are what an operator needs — and never the value, which does not
 * exist yet anyway.
 */
export async function resolveMcpSecrets(
  alias: string,
  secretRefs: Record<string, string> | undefined,
  store: SecretStore,
): Promise<McpSecretMaterial> {
  const entries = Object.entries(secretRefs ?? {});
  if (entries.length === 0) return EMPTY_SECRET_MATERIAL;

  const resolved = new Map<string, string>();
  for (const [placeholder, ref] of entries) {
    const value = await store.get(ref);
    if (value === null) {
      throw new McpError(
        "mcp_secret_missing",
        `MCP server "${alias}" needs secret ref "${ref}" for placeholder ` +
          `"\${${placeholder}}", but the secret store has no value for it.`,
        { details: { alias, placeholder, ref } },
      );
    }
    resolved.set(placeholder, value);
  }
  return createSecretMaterial(resolved);
}

/** Exported for tests and for hosts that resolve secrets themselves. */
export function createSecretMaterial(
  resolved: ReadonlyMap<string, string>,
): McpSecretMaterial {
  // Longest value first: redacting a token that CONTAINS a shorter one must not
  // leave the shorter one's suffix behind as readable text.
  const values = [...resolved.values()]
    .filter((value) => value.length > 0)
    .sort((a, b) => b.length - a.length);
  return {
    placeholders: new Set(resolved.keys()),
    substitute(value: string): string {
      let out = value;
      for (const [placeholder, secret] of resolved) {
        out = out.split(`\${${placeholder}}`).join(secret);
      }
      return out;
    },
    redact(text: string): string {
      let out = text;
      for (const secret of values) out = out.split(secret).join("***");
      return out;
    },
  };
}

/**
 * Apply substitution to the fields a config is allowed to carry secrets in:
 * stdio `env` values and http `headers` values. Commands, args and URLs are left
 * alone — a token in a process argv is visible in `ps`, and one in a URL lands
 * in every proxy log, so this package declines to help put it there.
 */
export function applySecretsToTransport(
  transport: McpTransportConfig,
  material: McpSecretMaterial,
): McpTransportConfig {
  if (transport.kind === "stdio") {
    if (!transport.env) return transport;
    return { ...transport, env: substituteValues(transport.env, material) };
  }
  if (!transport.headers) return transport;
  return { ...transport, headers: substituteValues(transport.headers, material) };
}

function substituteValues(
  record: Record<string, string>,
  material: McpSecretMaterial,
): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(record)) {
    out[key] = material.substitute(value);
  }
  return out;
}
