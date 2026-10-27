/**
 * Agent profiles: one runtime, many jobs (ADR 0015 §profiles).
 *
 * A profile is DATA, resolved by the host before a turn runs, and enforced where
 * enforcement lives — the registry, the guards and the budgets. It is never a
 * prompt-only promise: a profile that says `tools: ["read_file"]` and leaves
 * `edit_file` staged would be a profile that says nothing.
 *
 * That distinction is the whole file. `instructions` is advisory text the model
 * may ignore; `tools`, `permissionMode`, `role` and `limits` are host decisions
 * the model cannot talk its way out of, because the model never sees the tool
 * set the profile removed and cannot answer a guard.
 */
import type { AiReasoningEffort, RunContentSelection } from "@agentkit/core";
import {
  readSessionDescriptor,
  type AgentSessionDescriptor,
} from "../sessions/descriptor.js";
import { readSessionExecution } from "../sessions/plan-mode.js";
import type { AssistantStore } from "../ports/assistant-store.js";

/** What a profile may bound. Everything here is a host-side ceiling. */
export interface AgentLimits {
  /** Model/tool iterations before the run is stopped. */
  maxToolIterations?: number;
  /** Tool calls executed per iteration. */
  maxToolCallsPerIteration?: number;
  /** Conversation records replayed to the provider. */
  historyLimit?: number;
  /** Wall-clock budget for one attempt. */
  attemptBudgetMs?: number;
  /** Reply tokens requested from the provider. */
  maxOutputTokens?: number;
}

/** What a profile may pull in. Resources are selected, never discovered. */
export interface AgentResources {
  /** Skills carried into every turn of a session with this profile. */
  skills?: { id: string; revision: string }[];
  /** Load project instructions (AGENTS.md / CLAUDE.md) from the workspace. */
  projectInstructions?: boolean;
  /**
   * Extra tool names beyond the pack's own set, e.g. MCP tools by canonical id.
   * Still subject to {@link AgentProfile.tools}: a name here that is not in
   * `tools` buys nothing.
   */
  resources?: string[];
}

export type AgentProfileKind = "chat" | "coding" | "research" | "automation";

/** The coding roles. Each is a ceiling, not a costume. */
export type AgentRole = "implementer" | "reviewer" | "supervisor";

/** A profile: the whole configuration of one kind of agent. */
export interface AgentProfile {
  id: string;
  kind: AgentProfileKind;
  label?: string;
  /** Advisory system text. Enforced parts live elsewhere in this record. */
  instructions: string;
  model?: string;
  /** Provider-side thinking budget, where the model supports one. */
  reasoningEffort?: AiReasoningEffort;
  /**
   * The complete allowlist of tool names this profile may stage. Empty means
   * "no tools at all", not "no opinion" — a chat agent that silently kept the
   * shell is the failure this rule exists to prevent.
   */
  tools: readonly string[];
  /**
   * The permission mode the session starts in. The host may move it (a plan
   * approval does exactly that); the model may not.
   */
  permissionMode: string;
  /** Coding roles only: the class ceiling the host enforces. */
  role?: AgentRole;
  limits: AgentLimits;
  resources: AgentResources;
}

/**
 * The skills a turn carries: the session's own first, then the profile's.
 * An id already selected keeps its pinned revision, so a profile can add a
 * skill but never silently move one the session pinned.
 */
export function mergeProfileSkills(
  selection: RunContentSelection | undefined,
  profile: AgentProfile | undefined,
): RunContentSelection | undefined {
  const extra = profile?.resources.skills ?? [];
  if (extra.length === 0) return selection;
  const skills = [...(selection?.skills ?? [])];
  const seen = new Set(skills.map((skill) => skill.id));
  for (const skill of extra) {
    if (seen.has(skill.id)) continue;
    seen.add(skill.id);
    skills.push({ ...skill });
  }
  return { ...selection, skills };
}

/** The tool names `@agentkit/toolpacks-coding` contributes. */
export const CODING_TOOL_NAMES: readonly string[] = Object.freeze([
  "read_file",
  "list_dir",
  "find_files",
  "code_search",
  "repo_map",
  "git_inspect",
  "edit_file",
  "write_file",
  "run_command",
  "run_tests",
  "submit_plan",
]);

const READ_ONLY_TOOLS: readonly string[] = Object.freeze([
  "read_file",
  "list_dir",
  "find_files",
  "code_search",
  "repo_map",
  "git_inspect",
]);

const EXPLORE = READ_ONLY_TOOLS;

const CODING_INSTRUCTIONS =
  "Work in the granted workspace only. Explore with the native tools " +
  "(read_file, list_dir, find_files, code_search, repo_map, git_inspect) — " +
  "never with run_command. Change files with edit_file/write_file, and run the " +
  "test suite with run_tests. Keep changes minimal and verified.";

const RESEARCH_INSTRUCTIONS =
  "Answer from evidence you gathered in this run. Cite the file and line, or the " +
  "command output, for every claim. Prefer reading a file over guessing its " +
  "contents, and say what you could not determine.";

const AUTOMATION_INSTRUCTIONS =
  "Carry out the requested operation exactly, with the least scope that " +
  "accomplishes it. Report what changed. When an instruction is ambiguous and " +
  "the ambiguity has side effects, ask instead of choosing.";

const CHAT_INSTRUCTIONS =
  "Answer directly and honestly. Say what you do not know.";

/** The built-in profiles. A host can register its own over these. */
export const BUILT_IN_PROFILES: readonly AgentProfile[] = Object.freeze([
  {
    id: "chat",
    kind: "chat",
    label: "General chat agent",
    instructions: CHAT_INSTRUCTIONS,
    // No tools: a chat that quietly acquired a shell is a profile bug. The
    // supervisor ceiling is the backstop for a host that contributes tools and
    // forgets that this profile was supposed to have none.
    tools: [],
    permissionMode: "read_only",
    role: "supervisor",
    limits: Object.freeze({ maxToolIterations: 1, historyLimit: 40 }),
    resources: Object.freeze({ projectInstructions: false }),
  },
  {
    id: "coding",
    kind: "coding",
    label: "Coding agent",
    instructions: CODING_INSTRUCTIONS,
    tools: CODING_TOOL_NAMES,
    permissionMode: "ask",
    role: "implementer",
    limits: Object.freeze({
      maxToolIterations: 30,
      maxToolCallsPerIteration: 8,
      historyLimit: 200,
    }),
    resources: Object.freeze({
      projectInstructions: true,
    }),
  },
  {
    id: "coding/plan",
    kind: "coding",
    label: "Coding agent (plan first)",
    instructions:
      `${CODING_INSTRUCTIONS} Propose a plan and wait for approval before any ` +
      "file changes or commands.",
    tools: CODING_TOOL_NAMES,
    permissionMode: "plan",
    role: "implementer",
    limits: Object.freeze({
      maxToolIterations: 30,
      maxToolCallsPerIteration: 8,
      historyLimit: 200,
    }),
    resources: Object.freeze({ projectInstructions: true }),
  },
  {
    id: "coding/supervisor",
    kind: "coding",
    label: "Coding supervisor (read-only)",
    instructions:
      "Review and explain; never change anything. Read the code, describe the " +
      "problem precisely with file and line, and propose the change for someone " +
      "else to apply.",
    tools: READ_ONLY_TOOLS,
    permissionMode: "read_only",
    role: "supervisor",
    limits: Object.freeze({ maxToolIterations: 20, historyLimit: 200 }),
    resources: Object.freeze({ projectInstructions: true }),
  },
  {
    id: "coding/reviewer",
    kind: "coding",
    label: "Coding reviewer (may run tests, may not edit)",
    instructions:
      "Review by running, not by guessing: inspect the change, run the tests, " +
      "report findings with file and line. Do not edit files.",
    tools: Object.freeze([...READ_ONLY_TOOLS, "run_tests"]),
    permissionMode: "ask",
    role: "reviewer",
    limits: Object.freeze({ maxToolIterations: 20, historyLimit: 200 }),
    resources: Object.freeze({ projectInstructions: true }),
  },
  {
    id: "research",
    kind: "research",
    label: "Research agent",
    instructions: RESEARCH_INSTRUCTIONS,
    tools: Object.freeze([...EXPLORE, "run_command"]),
    permissionMode: "ask",
    // Reviewer, not implementer: a research agent that lists no write tools is
    // one careless profile edit away from having some, and the ceiling is what
    // makes that a refusal rather than a pull request.
    role: "reviewer",
    limits: Object.freeze({ maxToolIterations: 40, historyLimit: 300 }),
    resources: Object.freeze({ projectInstructions: true }),
  },
  {
    id: "automation",
    kind: "automation",
    label: "Automation agent",
    instructions: AUTOMATION_INSTRUCTIONS,
    tools: Object.freeze([
      ...EXPLORE,
      "edit_file",
      "write_file",
      "run_command",
    ]),
    // The point of `automation` is that nobody is watching: `ask` would park
    // every write on a human who never arrives. What stands in their place is
    // the workspace confinement, the role ceiling and these limits — so a host
    // schedules this profile on a workspace it has decided to trust.
    permissionMode: "full_auto",
    role: "implementer",
    limits: Object.freeze({
      maxToolIterations: 25,
      maxToolCallsPerIteration: 4,
      historyLimit: 100,
    }),
    resources: Object.freeze({ projectInstructions: false }),
  },
]);

/** A profile the host can rely on being there. */
export function defaultProfileRegistry(extra: readonly AgentProfile[] = []) {
  return createProfileRegistry([...extra, ...BUILT_IN_PROFILES]);
}

export interface ProfileRegistry {
  resolve(id: string): AgentProfile | undefined;
  list(): readonly AgentProfile[];
  /** The profile a session runs under, or the fallback when it has none. */
  forSession(
    descriptor: Pick<AgentSessionDescriptor, "profile"> | undefined,
  ): AgentProfile | undefined;
}

/**
 * A registry over a fixed list. Later entries win, so a host's own profiles
 * override a built-in of the same id rather than colliding with it.
 */
export function createProfileRegistry(
  profiles: readonly AgentProfile[],
): ProfileRegistry {
  const byId = new Map<string, AgentProfile>();
  for (const profile of profiles)
    byId.set(profile.id, validateProfile(profile));
  return {
    resolve: (id) => byId.get(id),
    list: () => [...byId.values()],
    forSession: (descriptor) =>
      descriptor === undefined ? undefined : byId.get(descriptor.profile.id),
  };
}

/** Throws on a profile the host cannot enforce; returns it unchanged. */
export function validateProfile(profile: AgentProfile): AgentProfile {
  const bad = (message: string): never => {
    throw new TypeError(
      `Agent profile "${profile.id ?? "(none)"}": ${message}`,
    );
  };
  if (typeof profile.id !== "string" || profile.id === "")
    bad("id must be a non-empty string");
  if (typeof profile.instructions !== "string")
    bad("instructions must be a string");
  if (
    !Array.isArray(profile.tools) ||
    profile.tools.some((t) => typeof t !== "string")
  )
    bad("tools must be an array of names");
  if (new Set(profile.tools).size !== profile.tools.length)
    bad("tools contains a duplicate");
  if (
    typeof profile.permissionMode !== "string" ||
    profile.permissionMode === ""
  )
    bad("permissionMode must be a non-empty string");
  if (profile.role !== undefined) {
    if (!["implementer", "reviewer", "supervisor"].includes(profile.role))
      bad(`role "${profile.role}" is not a known role`);
    // A role is a ceiling, and a ceiling a profile could widen by listing a
    // tool would not be one. The guard is still the enforcement; this is the
    // refusal to ship a profile that would fight it.
    if (profile.role === "supervisor") {
      const over = profile.tools.filter((name) => !isReadOnlyToolName(name));
      if (over.length > 0)
        bad(`role supervisor may not list non-read tools: ${over.join(", ")}`);
    }
    if (profile.role === "reviewer") {
      const over = profile.tools.filter(
        (name) => name === "edit_file" || name === "write_file",
      );
      if (over.length > 0)
        bad(`role reviewer may not list edit/write tools: ${over.join(", ")}`);
    }
  }
  if (
    profile.reasoningEffort !== undefined &&
    !["minimal", "low", "medium", "high"].includes(profile.reasoningEffort)
  )
    bad(`reasoningEffort "${profile.reasoningEffort}" is not a known level`);
  if (profile.limits !== undefined) {
    for (const [key, value] of Object.entries(profile.limits))
      if (value !== undefined && (!Number.isInteger(value) || value <= 0))
        bad(`limits.${key} must be a positive integer`);
    // setTimeout clamps anything above 2^31-1 ms to ~1 ms: a "very long"
    // budget would fire at once.
    const budgetMs = profile.limits.attemptBudgetMs;
    if (budgetMs !== undefined && budgetMs > 2_147_483_647)
      bad("limits.attemptBudgetMs must be at most 2147483647");
  }
  return profile;
}

/** Names that only ever read. Used to keep a supervisor profile honest. */
const MUTATION_HINTS = ["write", "edit", "run_", "apply", "delete", "remove"];

function isReadOnlyToolName(name: string): boolean {
  if (READ_ONLY_TOOLS.includes(name) || name === "submit_plan") return true;
  return !MUTATION_HINTS.some((hint) => name.includes(hint));
}

/**
 * The effective per-turn limits: the profile's, capped by the host's.
 *
 * A profile may ask for fewer iterations than the host allows; it may not ask
 * for more. That ordering is the difference between a profile and a privilege
 * escalation with a nicer name.
 */
export function effectiveLimits(
  profile: AgentProfile | undefined,
  host: AgentLimits,
): AgentLimits {
  const p = profile?.limits ?? {};
  const min = (
    a: number | undefined,
    b: number | undefined,
  ): number | undefined =>
    a === undefined ? b : b === undefined ? a : Math.min(a, b);
  return {
    maxToolIterations: min(p.maxToolIterations, host.maxToolIterations),
    maxToolCallsPerIteration: min(
      p.maxToolCallsPerIteration,
      host.maxToolCallsPerIteration,
    ),
    historyLimit: min(p.historyLimit, host.historyLimit),
    attemptBudgetMs: min(p.attemptBudgetMs, host.attemptBudgetMs),
    maxOutputTokens: min(p.maxOutputTokens, host.maxOutputTokens),
  };
}

/**
 * The permission authority a coding guard consults, driven by the session's
 * profile and its durable execution state.
 *
 * Precedence, highest first: the session's execution state (a plan approval, or
 * a mode the user set), then the profile, then the caller's fallback. The model
 * appears nowhere in that list, which is the point.
 */
export function createProfilePermissionAuthority<
  M extends string = string,
>(deps: {
  store: AssistantStore;
  profiles: ProfileRegistry;
  /**
   * Not an inference site: a caller that passes a literal must not narrow the
   * authority's whole mode type to that one string.
   */
  fallbackMode?: NoInfer<M>;
  fallbackRole?: AgentRole;
  resolveSessionId?: (ctx: {
    chatId?: string;
    runId?: string;
  }) => string | undefined | Promise<string | undefined>;
}): {
  modeOf(ctx: { chatId?: string; runId?: string }): Promise<M | undefined>;
  roleOf(ctx: {
    chatId?: string;
    runId?: string;
  }): Promise<AgentRole | undefined>;
  planApproved(ctx: { chatId?: string; planDigest: string }): Promise<boolean>;
} {
  const sessionIdOf = async (ctx: { chatId?: string; runId?: string }) =>
    deps.resolveSessionId ? await deps.resolveSessionId(ctx) : ctx.chatId;
  const lookUp = async (ctx: { chatId?: string; runId?: string }) => {
    const sessionId = await sessionIdOf(ctx);
    if (sessionId === undefined) return {};
    const chat = await deps.store.conversations.getChat(sessionId);
    if (chat === null) return {};
    const descriptor = readSessionDescriptor(chat.metadata);
    return {
      execution: readSessionExecution(chat.metadata),
      profile:
        descriptor === undefined
          ? undefined
          : deps.profiles.resolve(descriptor.profile.id),
    };
  };
  return {
    async modeOf(ctx) {
      const { execution, profile } = await lookUp(ctx);
      // A profile's mode is a string, because profiles are pack-agnostic; the
      // host that wires a pack to a profile is the one that knows which mode
      // names that pack understands.
      return (execution?.mode ??
        profile?.permissionMode ??
        deps.fallbackMode) as M | undefined;
    },
    async roleOf(ctx) {
      const { profile } = await lookUp(ctx);
      return profile?.role ?? deps.fallbackRole;
    },
    async planApproved(ctx) {
      const { execution } = await lookUp(ctx);
      return execution?.grantedPlan?.planDigest === ctx.planDigest;
    },
  };
}
