import type { McpTransportDto } from "@agentkit/contracts";
import type { SecretStore } from "@agentkit/host";

/** Normalize before durable config writes; a redacted edit preserves its prior value. */
export async function secretizeMcpTransport(
  id: string,
  input: McpTransportDto,
  inputRefs: Record<string, string> | undefined,
  secrets: SecretStore | undefined,
  previous?: McpTransportDto,
) {
  const transport = structuredClone(input);
  const secretRefs = { ...inputRefs };
  const values = transport.kind === "http" ? transport.headers : transport.env;
  const sameTarget =
    previous?.kind === transport.kind &&
    (transport.kind === "http"
      ? (previous as { url: string }).url === transport.url
      : (previous as { command: string }).command === transport.command);
  const oldValues = sameTarget
    ? previous?.kind === "http"
      ? previous.headers
      : previous?.env
    : undefined;
  for (const [key, original] of Object.entries(values ?? {})) {
    const value = original === "***" ? oldValues?.[key] : original;
    if (value === undefined)
      throw new Error(
        "Redacted credentials require the unchanged transport target and an existing value.",
      );
    if (/\$\{[^}]+\}/.test(value)) {
      values![key] = value;
      continue;
    }
    if (!secrets)
      throw new Error(
        "Literal MCP credentials require a configured SecretStore.",
      );
    const placeholder = `secret_${crypto.randomUUID().replaceAll("-", "")}`;
    const ref = `connections/${encodeURIComponent(id)}/${placeholder}`;
    await secrets.set(ref, value);
    secretRefs[placeholder] = ref;
    values![key] = `\${${placeholder}}`;
  }
  return { transport, secretRefs };
}
