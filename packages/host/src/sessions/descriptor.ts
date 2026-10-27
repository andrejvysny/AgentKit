/** Chat metadata key holding the {@link AgentSessionDescriptor} (ADR 0016 §1). */
export const SESSION_METADATA_KEY = "agentkit.session";
/**
 * Chat metadata key `AgentSessionService.compact` sets (an ISO timestamp); the
 * next session turn compacts below the threshold and clears it (ADR 0017 part 2).
 */
export const COMPACT_REQUESTED_METADATA_KEY =
  "agentkit.session.compactRequestedAt";

/** What a chat records once it is an agent session. */
export interface AgentSessionDescriptor {
  version: 1;
  profile: { id: string; digest?: string };
  providerId?: string;
  model?: string;
  createdAt: string;
  /** Project instructions source; absent means none are injected (ADR 0017). */
  workspace?: { root: string; trusted: boolean };
  /** Context assembly overrides (ADR 0017). */
  context?: SessionContextOptions;
  /** Skills re-selected on every turn of this session. */
  skills?: { id: string; revision: string }[];
}

export interface SessionContextOptions {
  /** `transcript` (default) replays tool calls and steering of prior turns. */
  policy?: "final_answers" | "transcript";
  /** Overrides the model's context window, in tokens. */
  windowTokens?: number;
  /** Tokens kept free for the reply. Default 16384, at most half the window. */
  reserveTokens?: number;
  /** Newest tokens left uncompacted when summarizing. Default 20000. */
  keepRecentTokens?: number;
  /**
   * Elision of old large tool results in the provider view (ADR 0017). Absent
   * uses the defaults; `false` sends every result whole.
   */
  toolResultElision?: { keepRecentTurns?: number; maxChars?: number } | false;
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
  const workspace = readWorkspace(raw["workspace"]);
  const context = readContext(raw["context"]);
  const skills = readSkills(raw["skills"]);
  if (workspace === null || context === null || skills === null)
    return undefined;
  return {
    version: 1,
    profile: { id, ...(digest === undefined ? {} : { digest }) },
    ...(providerId === undefined ? {} : { providerId }),
    ...(model === undefined ? {} : { model }),
    createdAt,
    ...(workspace === undefined ? {} : { workspace }),
    ...(context === undefined ? {} : { context }),
    ...(skills === undefined ? {} : { skills }),
  };
}

// Each reader answers undefined when the key is absent and null when it is
// present but malformed, so a bad optional field invalidates the descriptor
// the same way a bad `profile` does instead of being silently dropped.
function readWorkspace(
  raw: unknown,
): AgentSessionDescriptor["workspace"] | null | undefined {
  if (raw === undefined) return undefined;
  if (!isRecord(raw)) return null;
  const root = raw["root"];
  const trusted = raw["trusted"];
  if (typeof root !== "string" || root === "") return null;
  if (typeof trusted !== "boolean") return null;
  return { root, trusted };
}

function readPositiveInt(raw: unknown): number | null | undefined {
  if (raw === undefined) return undefined;
  return typeof raw === "number" && Number.isInteger(raw) && raw > 0
    ? raw
    : null;
}

function readContext(raw: unknown): SessionContextOptions | null | undefined {
  if (raw === undefined) return undefined;
  if (!isRecord(raw)) return null;
  const policy = raw["policy"];
  if (
    policy !== undefined &&
    policy !== "final_answers" &&
    policy !== "transcript"
  )
    return null;
  const windowTokens = readPositiveInt(raw["windowTokens"]);
  const reserveTokens = readPositiveInt(raw["reserveTokens"]);
  const keepRecentTokens = readPositiveInt(raw["keepRecentTokens"]);
  const toolResultElision = readElision(raw["toolResultElision"]);
  if (
    windowTokens === null ||
    reserveTokens === null ||
    keepRecentTokens === null ||
    toolResultElision === null
  )
    return null;
  return {
    ...(policy === undefined ? {} : { policy }),
    ...(windowTokens === undefined ? {} : { windowTokens }),
    ...(reserveTokens === undefined ? {} : { reserveTokens }),
    ...(keepRecentTokens === undefined ? {} : { keepRecentTokens }),
    ...(toolResultElision === undefined ? {} : { toolResultElision }),
  };
}

function readElision(
  raw: unknown,
): SessionContextOptions["toolResultElision"] | null | undefined {
  if (raw === undefined || raw === false) return raw;
  if (!isRecord(raw)) return null;
  const keepRecentTurns = readPositiveInt(raw["keepRecentTurns"]);
  const maxChars = readPositiveInt(raw["maxChars"]);
  if (keepRecentTurns === null || maxChars === null) return null;
  return {
    ...(keepRecentTurns === undefined ? {} : { keepRecentTurns }),
    ...(maxChars === undefined ? {} : { maxChars }),
  };
}

function readSkills(
  raw: unknown,
): { id: string; revision: string }[] | null | undefined {
  if (raw === undefined) return undefined;
  if (!Array.isArray(raw)) return null;
  const skills: { id: string; revision: string }[] = [];
  for (const entry of raw) {
    if (!isRecord(entry)) return null;
    const id = entry["id"];
    const revision = entry["revision"];
    if (typeof id !== "string" || typeof revision !== "string") return null;
    skills.push({ id, revision });
  }
  return skills;
}

/**
 * The run-content selection a session's turns carry: its carried-over Skills.
 * Undefined when there are none, so a session without Skills submits exactly
 * the payload it did before.
 */
export function sessionContentSelection(
  descriptor: AgentSessionDescriptor,
): { skills: { id: string; revision: string }[] } | undefined {
  if (descriptor.skills === undefined || descriptor.skills.length === 0)
    return undefined;
  return { skills: descriptor.skills.map((skill) => ({ ...skill })) };
}
