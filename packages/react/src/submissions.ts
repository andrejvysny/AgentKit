import { newIdempotencyKey, type AgentKitClient } from "@agentkit/client";
import type { SubmitMessageRequest } from "@agentkit/contracts";

type Submitted = Awaited<ReturnType<AgentKitClient["submitMessage"]>>;

interface Submission {
  signature: string;
  key: string;
  body: SubmitMessageRequest;
  pending: Promise<Submitted> | null;
}

// A client survives hook remounts. Unknown POST outcomes must survive with it,
// or reconnecting after acceptance can create a second logical turn.
const submissions = new WeakMap<AgentKitClient, Map<string, Submission>>();

function signature(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(signature).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const record = value as Record<string, unknown>;
    return `{${Object.keys(record)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${signature(record[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

export function submissionSignature(
  body: SubmitMessageRequest,
  key?: string,
): string {
  return signature([body, key ?? null]);
}

export function canReplaySubmission(
  client: AgentKitClient,
  chatId: string,
  body: SubmitMessageRequest,
  explicitKey?: string,
): boolean {
  const retained = submissions.get(client)?.get(chatId);
  return (
    retained?.signature === signature(JSON.parse(JSON.stringify(body))) &&
    (explicitKey === undefined || explicitKey === retained.key)
  );
}

export function submitOnce(
  client: AgentKitClient,
  chatId: string,
  body: SubmitMessageRequest,
  explicitKey?: string,
): Promise<Submitted> {
  let chats = submissions.get(client);
  if (chats === undefined) {
    chats = new Map();
    submissions.set(client, chats);
  }
  // Snapshot before the first await, including nested content and metadata.
  const snapshot = JSON.parse(JSON.stringify(body)) as SubmitMessageRequest;
  const nextSignature = signature(snapshot);
  let submission = chats.get(chatId);
  if (
    submission?.signature !== nextSignature ||
    (explicitKey !== undefined && explicitKey !== submission.key)
  ) {
    submission = {
      signature: nextSignature,
      key: explicitKey ?? newIdempotencyKey(),
      body: snapshot,
      pending: null,
    };
    chats.set(chatId, submission);
  }
  if (submission.pending !== null) return submission.pending;
  const retained = submission;
  const pending = client
    .submitMessage({ chatId }, retained.body, {
      idempotencyKey: retained.key,
    })
    .then((result) => {
      if (chats.get(chatId) === retained) chats.delete(chatId);
      return result;
    })
    .finally(() => {
      retained.pending = null;
    });
  retained.pending = pending;
  return pending;
}
