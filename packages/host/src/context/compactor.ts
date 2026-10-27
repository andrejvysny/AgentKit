/**
 * Session context compaction (ADR 0017 part 2): one tool-less summarization
 * request over the oldest turns of a transcript.
 *
 * The prompts, the conversation serialization (tool results cut to 2000
 * characters) and the read/modified-file tracking are adapted from Pi:
 *   upstream repository: earendil-works/pi (MIT, Copyright (c) 2025 Mario Zechner)
 *   upstream commit:     4a080252f06caf63a525d7873c5d94d3c23887e0 (v1.4.0)
 *   upstream paths:      packages/coding-agent/src/core/compaction/compaction.ts
 *                        (SUMMARIZATION_PROMPT, UPDATE_SUMMARIZATION_PROMPT,
 *                        findCutPoint), packages/coding-agent/src/core/compaction/utils.ts
 *                        (SUMMARIZATION_SYSTEM_PROMPT, serializeConversation,
 *                        file-operation tracking)
 * A selective TypeScript-strict rewrite over AgentKit's message shapes; the cut
 * is always at a turn start (never mid-turn) so a tool call and its results
 * stay together.
 *
 * The result is a DERIVED record. Nothing here writes to canonical history, and
 * every failure is a returned value, never a throw: the caller falls back to
 * trimming (M3a).
 */
import type {
  AiChatMessage,
  AiRunEvent,
  AiToolLimits,
} from "@agentkit/contracts";
import {
  AiToolRegistry,
  messageContentToText,
  runAgentLoop,
  type AiProviderClient,
  type TokenEstimator,
} from "@agentkit/core";
import {
  COMPACTION_SUMMARIZER_REVISION,
  type ContextSummaryRecord,
} from "../ports/context-summary-store.js";
import { sha256Hex } from "../sessions/hash.js";

type UsageEvent = Extract<AiRunEvent, { type: "run.usage" }>;

/** Used when the session descriptor sets no `context.keepRecentTokens`. */
export const DEFAULT_KEEP_RECENT_TOKENS = 20000;

/** Tool results longer than this are cut in the summarization request. */
export const TOOL_RESULT_MAX_CHARS = 2000;

const READ_TOOLS: ReadonlySet<string> = new Set(["read_file", "code_search"]);
const MODIFY_TOOLS: ReadonlySet<string> = new Set(["edit_file", "write_file"]);
const COMMAND_TOOLS: ReadonlySet<string> = new Set([
  "run_command",
  "run_tests",
]);
const MAX_COMMANDS = 20;
const MAX_COMMAND_CHARS = 200;

export const SUMMARIZATION_SYSTEM_PROMPT = `You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.`;

export const SUMMARIZATION_PROMPT = `The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.`;

const UPDATE_SUMMARIZATION_INSTRUCTIONS = `Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.`;

export const UPDATE_SUMMARIZATION_PROMPT = `The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

${UPDATE_SUMMARIZATION_INSTRUCTIONS}`;

/** The one user message that stands in for a summarized prefix. */
export function summaryMessage(summary: string): AiChatMessage {
  return {
    role: "user",
    content: `The conversation history before this point was compacted into the following summary:\n<summary>\n${summary}\n</summary>`,
  };
}

/** sha256 hex over (id, role, content, toolCallId) of each record, in order. */
export async function hashSource(
  messages: readonly AiChatMessage[],
  recordIds: readonly string[],
): Promise<string> {
  const rows = messages.map((message, index) => [
    recordIds[index] ?? "",
    message.role,
    message.content,
    message.toolCallId ?? null,
  ]);
  return sha256Hex(JSON.stringify(rows));
}

export interface AppliedSummary {
  messages: AiChatMessage[];
  recordIds: string[];
}

/**
 * Replace the covered prefix of `messages` (after an optional leading system
 * message) with the summary message. `recordIds` is parallel to the NON-system
 * messages. Returns undefined when the summary does not match this history:
 * either id is missing or out of order, the revision is not this build's, or
 * the covered records' hash differs (an edit, a branch switch, a fork).
 */
export async function applyValidSummary(
  messages: readonly AiChatMessage[],
  recordIds: readonly string[],
  summary: ContextSummaryRecord,
): Promise<AppliedSummary | undefined> {
  if (summary.summarizerRevision !== COMPACTION_SUMMARIZER_REVISION)
    return undefined;
  const through = recordIds.indexOf(summary.coversThroughMessageId);
  const kept = recordIds.indexOf(summary.firstKeptMessageId);
  if (through < 0 || kept !== through + 1) return undefined;
  const bodyStart = messages[0]?.role === "system" ? 1 : 0;
  const body = messages.slice(bodyStart);
  const hash = await hashSource(
    body.slice(0, through + 1),
    recordIds.slice(0, through + 1),
  );
  if (hash !== summary.sourceHash) return undefined;
  return {
    messages: [
      ...messages.slice(0, bodyStart),
      summaryMessage(summary.summary),
      ...body.slice(kept),
    ],
    recordIds: [`summary:${summary.summaryId}`, ...recordIds.slice(kept)],
  };
}

function startsTurn(body: readonly AiChatMessage[], index: number): boolean {
  return body[index]?.role === "user" && body[index - 1]?.role !== "tool";
}

export interface CutResult {
  /** Body index of the first kept message (a turn start). */
  keptFrom: number;
}

/**
 * Keep the newest whole turns whose estimate fits `keepRecentTokens`; the
 * current (last) turn is always kept. `from` is the first body index that may
 * still be covered (just past a previous summary).
 */
export function findCut(
  body: readonly AiChatMessage[],
  from: number,
  keepRecentTokens: number,
  estimator: TokenEstimator,
): number {
  const starts: number[] = [];
  for (let index = from; index < body.length; index += 1)
    if (startsTurn(body, index)) starts.push(index);
  if (starts.length === 0) return from;
  const cost = (begin: number, end: number): number => {
    let total = 0;
    for (let index = begin; index < end; index += 1)
      total += estimator.estimateMessage(body[index] as AiChatMessage);
    return total;
  };
  let kept = starts.length - 1;
  let tokens = cost(starts[kept] as number, body.length);
  while (kept > 0) {
    const turn = cost(starts[kept - 1] as number, starts[kept] as number);
    if (tokens + turn > keepRecentTokens) break;
    tokens += turn;
    kept -= 1;
  }
  return starts[kept] as number;
}

function truncateForSummary(text: string, maxChars: number): string {
  if (text.length <= maxChars) return text;
  return `${text.slice(0, maxChars)}\n\n[... ${text.length - maxChars} more characters truncated]`;
}

function describeArguments(argumentsJson: string): string {
  try {
    const parsed: unknown = JSON.parse(argumentsJson);
    if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed))
      return Object.entries(parsed)
        .map(([key, value]) => `${key}=${JSON.stringify(value)}`)
        .join(", ");
  } catch {
    // Unparseable arguments are shown as the model wrote them.
  }
  return argumentsJson;
}

/** Pi-style transcript: `[User]:`, `[Assistant]:`, `[Assistant tool calls]:`, `[Tool result]:`. */
export function serializeConversation(
  messages: readonly AiChatMessage[],
): string {
  const parts: string[] = [];
  for (const message of messages) {
    const text = messageContentToText(message.content);
    if (message.role === "user") {
      if (text) parts.push(`[User]: ${text}`);
    } else if (message.role === "assistant") {
      if (text) parts.push(`[Assistant]: ${text}`);
      const calls = (message.toolCalls ?? []).map(
        (call) => `${call.name}(${describeArguments(call.argumentsJson)})`,
      );
      if (calls.length > 0)
        parts.push(`[Assistant tool calls]: ${calls.join("; ")}`);
    } else if (message.role === "tool") {
      if (text)
        parts.push(
          `[Tool result]: ${truncateForSummary(text, TOOL_RESULT_MAX_CHARS)}`,
        );
    }
  }
  return parts.join("\n\n");
}

/** Commands run by `run_command` / `run_tests`, in call order, each cut short. */
export function extractCommands(messages: readonly AiChatMessage[]): string[] {
  const commands: string[] = [];
  for (const message of messages) {
    for (const call of message.toolCalls ?? []) {
      if (!COMMAND_TOOLS.has(call.name)) continue;
      try {
        const command = (
          JSON.parse(call.argumentsJson) as { command?: unknown }
        ).command;
        if (typeof command === "string" && command !== "")
          commands.push(command.slice(0, MAX_COMMAND_CHARS));
      } catch {}
    }
  }
  return commands;
}

const COMMANDS_BLOCK = /<commands-run>\n([\s\S]*?)\n<\/commands-run>/;

/** The previous summary is the only place earlier commands survive. */
function commandsOf(summary: string | undefined): string[] {
  const block = summary === undefined ? null : COMMANDS_BLOCK.exec(summary);
  return block?.[1] === undefined ? [] : block[1].split("\n");
}

export function extractFileDetails(messages: readonly AiChatMessage[]): {
  readFiles: string[];
  modifiedFiles: string[];
} {
  const read = new Set<string>();
  const modified = new Set<string>();
  for (const message of messages) {
    for (const call of message.toolCalls ?? []) {
      const isRead = READ_TOOLS.has(call.name);
      if (!isRead && !MODIFY_TOOLS.has(call.name)) continue;
      let path: unknown;
      try {
        path = (JSON.parse(call.argumentsJson) as { path?: unknown }).path;
      } catch {
        continue;
      }
      if (typeof path !== "string" || path === "") continue;
      (isRead ? read : modified).add(path);
    }
  }
  return {
    readFiles: [...read].filter((path) => !modified.has(path)).sort(),
    modifiedFiles: [...modified].sort(),
  };
}

function formatFileOperations(
  details: {
    readFiles: string[];
    modifiedFiles: string[];
  },
  commands: readonly string[],
): string {
  const sections: string[] = [];
  if (details.readFiles.length > 0)
    sections.push(
      `<read-files>\n${details.readFiles.join("\n")}\n</read-files>`,
    );
  if (details.modifiedFiles.length > 0)
    sections.push(
      `<modified-files>\n${details.modifiedFiles.join("\n")}\n</modified-files>`,
    );
  if (commands.length > 0)
    sections.push(`<commands-run>\n${commands.join("\n")}\n</commands-run>`);
  return sections.length === 0 ? "" : `\n\n${sections.join("\n\n")}`;
}

export interface CompactSessionContextInput {
  sessionId: string;
  /** Transcript-assembled messages: an optional leading system message, then the body. */
  messages: readonly AiChatMessage[];
  /** Canonical record id per NON-system message, parallel to the body. */
  recordIds: readonly string[];
  client: AiProviderClient;
  model: string;
  providerId?: string;
  estimator: TokenEstimator;
  windowTokens: number;
  reserveTokens: number;
  keepRecentTokens: number;
  limits: AiToolLimits;
  /** The newest VALID summary for this history, when one exists. */
  previous?: ContextSummaryRecord;
  signal?: AbortSignal;
  /** The task the compaction belongs to; `runId` is `<taskId>:compaction:<n>`. */
  taskId: string;
  compactionNumber: number;
  summaryId: string;
  nowIso: string;
}

export type CompactionOutcome =
  | {
      ok: true;
      record: ContextSummaryRecord;
      /** The summarization request's own `run.usage` events. */
      usage: UsageEvent[];
      coveredMessages: number;
    }
  | { ok: false; reason: string };

/**
 * Summarize the oldest turns of a transcript. Resolves `undefined` when there
 * is nothing to cover (everything left is the kept tail), a `{ ok: false }`
 * value on any failure, and never throws except on abort.
 */
export async function compactSessionContext(
  input: CompactSessionContextInput,
): Promise<CompactionOutcome | undefined> {
  const bodyStart = input.messages[0]?.role === "system" ? 1 : 0;
  const body = input.messages.slice(bodyStart);
  let from = 0;
  if (input.previous !== undefined) {
    const kept = input.recordIds.indexOf(input.previous.firstKeptMessageId);
    if (kept < 0) return { ok: false, reason: "previous summary not in path" };
    from = kept;
  }
  const keptFrom = findCut(body, from, input.keepRecentTokens, input.estimator);
  if (keptFrom <= from) return undefined;

  const covered = body.slice(from, keptFrom);
  const lastCovered = input.recordIds[keptFrom - 1];
  const firstKept = input.recordIds[keptFrom];
  if (lastCovered === undefined || firstKept === undefined)
    return { ok: false, reason: "record ids do not match messages" };

  const prompt =
    input.previous === undefined
      ? SUMMARIZATION_PROMPT
      : UPDATE_SUMMARIZATION_PROMPT;
  const previousBlock =
    input.previous === undefined
      ? ""
      : `\n\n<previous-summary>\n${input.previous.summary}\n</previous-summary>`;
  // The request has to fit the window itself: keep the newest part of an
  // oversized covered range rather than send something the provider refuses.
  const charBudget = Math.max(
    2000,
    (input.windowTokens - input.reserveTokens) * 4 -
      prompt.length -
      previousBlock.length -
      SUMMARIZATION_SYSTEM_PROMPT.length,
  );
  let conversation = serializeConversation(covered);
  if (conversation.length > charBudget)
    conversation = `[... earlier conversation omitted]\n\n${conversation.slice(-charBudget)}`;

  const messages: AiChatMessage[] = [
    { role: "system", content: SUMMARIZATION_SYSTEM_PROMPT },
    {
      role: "user",
      content: `<conversation>\n${conversation}\n</conversation>${previousBlock}\n\n${prompt}`,
    },
  ];

  const usage: UsageEvent[] = [];
  let finishReason: string | undefined;
  let text = "";
  try {
    const run = runAgentLoop({
      client: input.client,
      model: input.model,
      messages,
      registry: new AiToolRegistry(),
      limits: input.limits,
      maxToolIterations: 1,
      maxOutputTokens: Math.max(512, Math.floor(input.reserveTokens * 0.8)),
      ...(input.signal === undefined ? {} : { signal: input.signal }),
      runId: `${input.taskId}:compaction:${input.compactionNumber}`,
    });
    let next = await run.next();
    while (!next.done) {
      const event = next.value;
      if (event.type === "run.usage") usage.push(event);
      else if (event.type === "run.completed")
        finishReason = event.data.finishReason;
      next = await run.next();
    }
    if (next.value.terminal !== "completed")
      return { ok: false, reason: `summarizer ${next.value.terminal}` };
    const last = [...next.value.appendedMessages]
      .reverse()
      .find((message) => message.role === "assistant");
    text = last === undefined ? "" : messageContentToText(last.content).trim();
  } catch (error) {
    if (input.signal?.aborted) throw error;
    return { ok: false, reason: "summarizer request threw" };
  }
  if (text === "") return { ok: false, reason: "summarizer returned no text" };
  if (finishReason === "length" || finishReason === "incomplete")
    return { ok: false, reason: `summary incomplete (${finishReason})` };

  const found = extractFileDetails(covered);
  const modified = new Set([
    ...(input.previous?.details.modifiedFiles ?? []),
    ...found.modifiedFiles,
  ]);
  const details = {
    readFiles: [
      ...new Set([
        ...(input.previous?.details.readFiles ?? []),
        ...found.readFiles,
      ]),
    ]
      .filter((path) => !modified.has(path))
      .sort(),
    modifiedFiles: [...modified].sort(),
  };
  const commands = [
    ...commandsOf(input.previous?.summary),
    ...extractCommands(covered),
  ].slice(-MAX_COMMANDS);
  const summary = `${text}${formatFileOperations(details, commands)}`;
  const coveredAll = body.slice(0, keptFrom);
  const record: ContextSummaryRecord = {
    summaryId: input.summaryId,
    sessionId: input.sessionId,
    coversThroughMessageId: lastCovered,
    firstKeptMessageId: firstKept,
    summary,
    sourceHash: await hashSource(
      coveredAll,
      input.recordIds.slice(0, keptFrom),
    ),
    tokensBefore: coveredAll.reduce(
      (sum, message) => sum + input.estimator.estimateMessage(message),
      0,
    ),
    tokensAfter: input.estimator.estimateMessage(summaryMessage(summary)),
    ...(input.providerId === undefined ? {} : { providerId: input.providerId }),
    model: input.model,
    summarizerRevision: COMPACTION_SUMMARIZER_REVISION,
    details,
    createdAt: input.nowIso,
  };
  return { ok: true, record, usage, coveredMessages: coveredAll.length };
}
