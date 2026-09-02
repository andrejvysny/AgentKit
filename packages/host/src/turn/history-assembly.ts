/**
 * Building the provider-facing conversation for one pass: replaying stored
 * records into `AiChatMessage`s ({@link assembleMessages}), resolving `ref`
 * image sources into bytes under a budget ({@link resolveAttachments}), and
 * the crude prompt-size estimate {@link UsageAuthorizer} consults. Split out
 * of `turn-runner.ts` because none of it touches a provider or the task log —
 * it only turns stored state into the messages a pass sends.
 */
import type {
  AiChatMessage,
  AiContentPart,
  AiMessageContent,
} from "@agentkit/contracts";
import { messageContentToText } from "@agentkit/core";
import type {
  AttachmentResolver,
  ResolvedAttachment,
} from "../ports/attachment-resolver.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import type { ResolvedHookTimeouts } from "./hook-deadline.js";
import { isHookTimeout, withHookDeadline } from "./hook-deadline.js";
import { reconcileOrphanToolCalls } from "./history-reconcile.js";
import { orderMessagesForProvider } from "./message-order.js";
import type { TurnRunnerDeps } from "./turn-runner.js";

/** Default number of messages replayed to the provider. */
const DEFAULT_HISTORY_LIMIT = 200;

/**
 * Default attachment budgets for ONE provider pass.
 *
 * Borrowed from OpenPCB's `MENTION_LIMITS`, which are the numbers a shipping
 * product arrived at against real vision models rather than a guess: 5 MiB is
 * comfortably above a full-resolution screenshot and below the request size
 * providers start rejecting; 20 MiB and 16 images bound what a long conversation
 * full of attachments can cost on EVERY pass, since history is replayed whole.
 *
 * They are ceilings on what the {@link AttachmentResolver} contributes, not on
 * the request: a caller that inlines its own base64 `data` sources has already
 * decided how big its messages are, and second-guessing that here would drop
 * images the host never asked this port about.
 */
const DEFAULT_MAX_BYTES_PER_IMAGE = 5 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES = 20 * 1024 * 1024;
const DEFAULT_MAX_IMAGES = 16;

/**
 * Caps on what resolving attachments may add to one provider pass. Every field
 * is optional and falls back to the constant above.
 */
export interface AttachmentBudgets {
  /** Decoded bytes one image may contribute. Default 5 MiB. */
  maxBytesPerImage?: number;
  /** Decoded bytes ALL resolved images may contribute to one pass. Default 20 MiB. */
  maxTotalBytes?: number;
  /** How many resolved images one pass may carry. Default 16. */
  maxImages?: number;
}

interface ResolvedBudgets {
  maxBytesPerImage: number;
  maxTotalBytes: number;
  maxImages: number;
}

/**
 * Decoded size of a base64 payload, without decoding it.
 *
 * Four base64 characters encode three bytes, so `length * 3 / 4` is the size to
 * within the two padding characters — an over-estimate by at most 2 B, on a
 * budget measured in mebibytes. Decoding to find out exactly would allocate the
 * whole image to answer a question about whether to allocate the whole image.
 */
function decodedByteLength(base64: string): number {
  return Math.floor((base64.length * 3) / 4);
}

/** Whether a message body carries at least one `ref`-sourced image part. */
function hasRefImage(content: AiMessageContent): boolean {
  return (
    typeof content !== "string" &&
    content.some((part) => part.type === "image" && part.source.kind === "ref")
  );
}

/**
 * Why a resolved attachment may not join this pass, or `null` when it may.
 *
 * The three caps are checked in order of how local the failure is — this
 * image's own size, then what the pass has already spent, then how many images
 * it already carries — so the reason a caller is told is the most specific one
 * that applies. The returned string is the tail of the warning message, and it
 * names the number that was hit: "over budget" with no figure in it is a
 * warning nobody can act on.
 */
function budgetRefusal(input: {
  bytes: number;
  totalBytes: number;
  images: number;
  budgets: ResolvedBudgets;
}): string | null {
  const { bytes, totalBytes, images, budgets } = input;
  if (bytes > budgets.maxBytesPerImage) {
    return `its ${bytes} decoded bytes exceed the ${budgets.maxBytesPerImage}-byte per-image budget.`;
  }
  if (totalBytes + bytes > budgets.maxTotalBytes) {
    return `its ${bytes} decoded bytes would push this pass past the ${budgets.maxTotalBytes}-byte total budget (${totalBytes} already used).`;
  }
  if (images + 1 > budgets.maxImages) {
    return `this pass already carries its budgeted ${budgets.maxImages} image(s).`;
  }
  return null;
}

function resolveBudgets(deps: TurnRunnerDeps): ResolvedBudgets {
  const configured = deps.attachmentBudgets;
  return {
    maxBytesPerImage:
      configured?.maxBytesPerImage ?? DEFAULT_MAX_BYTES_PER_IMAGE,
    maxTotalBytes: configured?.maxTotalBytes ?? DEFAULT_MAX_TOTAL_BYTES,
    maxImages: configured?.maxImages ?? DEFAULT_MAX_IMAGES,
  };
}

/**
 * Everything {@link resolveAttachments} needs from `TurnRunner` besides the
 * deps it already carries: the resolved attachment deadline, and the one
 * private method it reports drops through — kept as a callback rather than a
 * `deps` field, since emitting a warning touches the task's durable log.
 */
export interface AttachmentContext {
  deps: TurnRunnerDeps;
  hookTimeouts: Pick<ResolvedHookTimeouts, "attachments">;
  emitWarning(
    ctx: TaskExecutionContext,
    code: string,
    message: string,
  ): Promise<void>;
}

/**
 * One `resolve()` under the attachment deadline; a timeout answers `null`.
 *
 * Only the DEADLINE is folded — a resolver that THROWS still throws, exactly
 * as before, because a host that raised an authorization error from this port
 * meant the turn to stop. What is folded is the one outcome that used to have
 * no answer at all.
 */
async function resolveAttachmentQuietly(
  context: AttachmentContext,
  resolver: AttachmentResolver,
  ref: string,
  chatId: string,
): Promise<ResolvedAttachment | null> {
  try {
    return await withHookDeadline({
      hook: "attachments.resolve",
      timeoutMs: context.hookTimeouts.attachments,
      run: () => resolver.resolve(ref, { chatId }),
    });
  } catch (err) {
    if (!isHookTimeout(err)) throw err;
    context.deps.logger?.warn("attachment resolver timed out", {
      chatId,
      ref,
      timeoutMs: context.hookTimeouts.attachments,
    });
    return null;
  }
}

/**
 * Replace every `ref` image source in this pass's history with the bytes
 * behind it — in memory, for this pass only.
 *
 * WHAT IS NOT TOUCHED: the stored message. A ref is what the conversation
 * holds, and re-resolving it on the next pass is the whole point — an
 * attachment can be revoked, replaced, or become too large for a budget that
 * changed, and a record that had already been rewritten to base64 could not
 * notice any of it. Nothing here writes to `ConversationStore`.
 *
 * WHAT A DROP LOOKS LIKE. An image that cannot be sent is removed from the
 * message the provider sees, and the turn continues with the words around it —
 * the same "degrade, never fail a request over an attachment" rule the
 * provider client follows when it flattens parts on a `system` message. Each
 * dropped part gets exactly one durable `run.warning` naming its ref and why,
 * so a UI can say "this image was not sent" instead of quietly answering a
 * question about a picture the model never saw. A message whose parts are ALL
 * dropped becomes the empty STRING rather than an empty array: `content: []`
 * is a shape the contract rejects and providers reject.
 *
 * THE CACHE IS PER PASS, deliberately. The same ref mentioned twice in one
 * history costs one `resolve()`; the retry pass that follows a failed one asks
 * again, because "these bytes are still there" is not a fact that survives an
 * arbitrary amount of time and a provider round-trip.
 *
 * A history with no refs in it returns the caller's own array untouched — the
 * overwhelmingly common case allocates nothing and asks the port nothing.
 */
export async function resolveAttachments(
  context: AttachmentContext,
  ctx: TaskExecutionContext,
  chatId: string,
  messages: readonly AiChatMessage[],
): Promise<readonly AiChatMessage[]> {
  if (!messages.some((message) => hasRefImage(message.content))) {
    return messages;
  }
  const budgets = resolveBudgets(context.deps);
  const resolver = context.deps.attachments;
  const cache = new Map<string, ResolvedAttachment | null>();
  let totalBytes = 0;
  let images = 0;

  const resolved: AiChatMessage[] = [];
  for (const message of messages) {
    if (!hasRefImage(message.content)) {
      resolved.push(message);
      continue;
    }
    const parts: AiContentPart[] = [];
    // `content` is narrowed to a parts array by `hasRefImage`.
    for (const part of message.content as AiContentPart[]) {
      if (part.type !== "image" || part.source.kind !== "ref") {
        parts.push(part);
        continue;
      }
      const ref = part.source.ref;
      if (resolver === undefined) {
        await context.emitWarning(
          ctx,
          "attachment_unresolved",
          `Attachment "${ref}" was dropped from this pass: no AttachmentResolver is wired.`,
        );
        continue;
      }
      if (!cache.has(ref)) {
        // The chat is passed so a multi-tenant host can SCOPE the lookup:
        // refs come from the client, so "may this chat see it" is the actual
        // question — see {@link AttachmentResolver.resolve}.
        //
        // UNDER A DEADLINE, and a timeout reads as "no bytes": a resolver
        // that has not answered is not a resolver that said yes, and the
        // drop path below is exactly the degradation this port already
        // documents. The `null` is cached with it, so a history mentioning
        // the same ref five times waits once rather than five times.
        cache.set(
          ref,
          await resolveAttachmentQuietly(context, resolver, ref, chatId),
        );
      }
      const attachment = cache.get(ref) ?? null;
      if (attachment === null) {
        await context.emitWarning(
          ctx,
          "attachment_unresolved",
          `Attachment "${ref}" was dropped from this pass: the resolver has no bytes for it.`,
        );
        continue;
      }
      const bytes = decodedByteLength(attachment.base64);
      const refusal = budgetRefusal({
        bytes,
        totalBytes,
        images,
        budgets,
      });
      if (refusal !== null) {
        await context.emitWarning(
          ctx,
          "attachment_budget_exceeded",
          `Attachment "${ref}" was dropped from this pass: ${refusal}`,
        );
        continue;
      }
      totalBytes += bytes;
      images += 1;
      parts.push({
        ...part,
        source: {
          kind: "data",
          base64: attachment.base64,
          mediaType: attachment.mediaType,
        },
      });
    }
    resolved.push({ ...message, content: parts.length === 0 ? "" : parts });
  }
  return resolved;
}

/**
 * Assemble the provider-facing conversation from stored records.
 *
 * The placeholder is skipped — it is where this turn's answer is being
 * written, and feeding a model its own empty (or half-written) reply is how a
 * turn ends up completing someone else's sentence. `role: "system"` records
 * are skipped too: those are UI banners the host wrote about the turn, not
 * prompt material — as are the correction harness's write-backs, which were
 * instructions to one pass of one run and not standing orders (below).
 *
 * The records come from `listMessages`, which reports the chat's ACTIVE PATH —
 * so a branch submit replays the branch and not the answer it replaced, with
 * no branch filtering needed here. `orderMessagesForProvider` runs over them
 * unchanged: `orderKey` increases with depth along any path, so ordering the
 * active path by `orderKey` and ordering it by depth are the same order, and
 * the run-scoped tool-call linkage it restores is untouched by branching.
 *
 * The result is then balanced in BOTH directions before it leaves: a tool
 * result whose requesting turn fell outside the window is dropped (below), and
 * a tool call whose result never got written is answered with a synthetic
 * failure (see {@link reconcileOrphanToolCalls}). Either imbalance is rejected
 * outright by every provider, and both are reachable — the first from the
 * history limit, the second from a turn that died between two writes.
 */
export async function assembleMessages(
  deps: TurnRunnerDeps,
  chatId: string,
  assistantMessageId: string,
  systemPrompt: string | null,
): Promise<AiChatMessage[]> {
  const records = await deps.store.conversations.listMessages(chatId, {
    limit: deps.historyLimit ?? DEFAULT_HISTORY_LIMIT,
  });
  const ordered = orderMessagesForProvider(records);
  const messages: AiChatMessage[] = [];
  if (systemPrompt) messages.push({ role: "system", content: systemPrompt });
  /**
   * Tool call ids an assistant turn in THIS window actually declared. A tool
   * result whose requesting turn fell outside the history limit is an orphan
   * `tool_call_id`, which providers reject outright — so the window silently
   * moving past an assistant turn must not take the whole conversation down
   * with it.
   */
  const declaredToolCallIds = new Set<string>();
  for (const record of ordered) {
    if (record.id === assistantMessageId) continue;
    // A correction write-back is an instruction the harness aimed at ONE pass
    // of ONE run ("fix these three items now, by calling your tools"). It is
    // persisted for the audit trail — the stored history has to say why the
    // model changed its answer — but replaying it here would hand every later
    // turn a dangling order about deficiencies that were already addressed,
    // with nothing left in view to address. The harness's own passes are
    // unaffected: they build their messages directly, not from this history.
    if (record.metadata["correctionPass"] !== undefined) continue;
    if (record.role === "user") {
      messages.push({ role: "user", content: record.content });
    } else if (record.role === "assistant") {
      for (const call of record.toolCalls ?? []) {
        declaredToolCallIds.add(call.id);
      }
      messages.push({
        role: "assistant",
        content: record.content,
        ...(record.toolCalls && record.toolCalls.length > 0
          ? { toolCalls: record.toolCalls }
          : {}),
      });
    } else if (
      record.role === "tool" &&
      record.toolCallId !== undefined &&
      declaredToolCallIds.has(record.toolCallId)
    ) {
      const toolName = record.metadata["toolName"];
      messages.push({
        role: "tool",
        content: record.content,
        toolCallId: record.toolCallId,
        ...(typeof toolName === "string" ? { name: toolName } : {}),
      });
    }
  }
  return reconcileOrphanToolCalls(messages);
}

/**
 * The request an assembled history is asking about: its LAST `role: "user"`
 * message, as text. Null when there is none, or when it carries no text.
 *
 * The last rather than the first, because a chat is a sequence of questions and
 * the one being answered is the most recent — every earlier user turn already
 * has its answer in the history. Text only, via `messageContentToText`: the one
 * caller is the correction harness, whose messages are built rather than
 * assembled, so an image part here would reach the provider as an unresolved
 * `ref`. See {@link CorrectionMessagesInput.userRequest}.
 */
export function lastUserRequestOf(
  messages: readonly AiChatMessage[],
): string | null {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "user") continue;
    const text = messageContentToText(message.content);
    return text.trim().length > 0 ? text : null;
  }
  return null;
}

/**
 * A prompt-size estimate for {@link UsageAuthorizer.authorize}: the assembled
 * conversation's characters, divided by four.
 *
 * Four characters per token is the rule of thumb, not a measurement — a real
 * count needs the provider's tokenizer, which this layer deliberately does not
 * carry (it would mean shipping a tokenizer per provider, kept in step with each
 * one's releases, to compute a number the port documents as best-effort). Image
 * and other non-text parts contribute nothing, via `messageContentToText`: their
 * token cost is provider-specific and guessing it would be a worse number than
 * admitting it is missing. `estimatedPromptTokens` is optional on the port
 * precisely so a host with a better estimate can ignore this one.
 */
export function estimatePromptTokens(
  messages: readonly AiChatMessage[],
): number {
  let chars = 0;
  for (const message of messages) {
    chars += messageContentToText(message.content).length;
  }
  return Math.ceil(chars / 4);
}
