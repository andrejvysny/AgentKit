/** Chat metadata key holding the {@link AgentSessionDescriptor} (ADR 0016 §1). */
export const SESSION_METADATA_KEY = "agentkit.session";

/** What a chat records once it is an agent session. */
export interface AgentSessionDescriptor {
  version: 1;
  profile: { id: string; digest?: string };
  providerId?: string;
  model?: string;
  createdAt: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The chat's session descriptor, or undefined when it is absent or malformed. */
export function readSessionDescriptor(
  metadata: Record<string, unknown> | undefined,
): AgentSessionDescriptor | undefined {
  const raw = metadata?.[SESSION_METADATA_KEY];
  if (!isRecord(raw) || raw["version"] !== 1) return undefined;
  const profile = raw["profile"];
  if (!isRecord(profile)) return undefined;
  const id = profile["id"];
  const digest = profile["digest"];
  const providerId = raw["providerId"];
  const model = raw["model"];
  const createdAt = raw["createdAt"];
  if (typeof id !== "string" || id === "") return undefined;
  if (digest !== undefined && typeof digest !== "string") return undefined;
  if (providerId !== undefined && typeof providerId !== "string")
    return undefined;
  if (model !== undefined && typeof model !== "string") return undefined;
  if (typeof createdAt !== "string") return undefined;
  return {
    version: 1,
    profile: { id, ...(digest === undefined ? {} : { digest }) },
    ...(providerId === undefined ? {} : { providerId }),
    ...(model === undefined ? {} : { model }),
    createdAt,
  };
}
