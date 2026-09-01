/**
 * `useProposals` — the staged writes a chat is waiting on a human for.
 *
 * A proposal is the host's answer to "the model wants to change something you
 * care about": the write is built, validated and PARKED, and nothing happens
 * until somebody approves it and something applies it. The three verbs are
 * separate on purpose — approving records a decision, applying performs the
 * side effect — and this hook keeps them separate rather than folding them into
 * one convenient button.
 *
 * WHY IT SUBSCRIBES. Proposals appear as a side effect of a RUN, not of
 * anything this hook did: the tool call that stages one happens mid-turn, and
 * the queue is only worth re-reading once the turn is over. `useChat`
 * invalidates the chat topic when a run reaches its terminal event, and that is
 * what refreshes this list — no polling, no `refetchInterval`.
 */
import {
  newIdempotencyKey,
  type AgentKitClient,
  type AgentKitClientError,
} from "@agentkit/client";
import type { ProposalDto, ProposalStatusDto } from "@agentkit/contracts";
import { useCallback, useEffect } from "react";
import { useAgentKitClient, useAgentKitContext } from "./context.js";
import { chatTopic } from "./emitter.js";
import { isAbort, toError, useAliveRef, useMirroredState } from "./internal.js";
import { useTopicSubscription } from "./internal.js";

export interface ProposalsState {
  proposals: ProposalDto[];
  loading: boolean;
  /** A decision or an apply is in flight. */
  busy: boolean;
  error: AgentKitClientError | Error | null;
}

export interface UseProposalsOptions {
  client?: AgentKitClient;
  /** Only proposals in this state. Omit for every state the host keeps. */
  status?: ProposalStatusDto;
  limit?: number;
}

export interface UseProposalsResult extends ProposalsState {
  approve(proposalId: string, reason?: string): Promise<ProposalDto | null>;
  reject(proposalId: string, reason?: string): Promise<ProposalDto | null>;
  /**
   * Perform the write.
   *
   * `operationId` is the CLIENT's idempotency key for the side effect: replaying
   * one returns the recorded outcome instead of writing twice. One is minted
   * per call when none is given, which is right for a button press and wrong
   * for a retry — a retry must pass the key the failed attempt used.
   */
  apply(proposalId: string, operationId?: string): Promise<ProposalDto | null>;
  reload(): Promise<void>;
}

const EMPTY: ProposalsState = {
  proposals: [],
  loading: false,
  busy: false,
  error: null,
};

export function useProposals(
  chatId: string | null,
  options: UseProposalsOptions = {},
): UseProposalsResult {
  const { emitter } = useAgentKitContext();
  const client = useAgentKitClient(options.client);
  const alive = useAliveRef();
  const { value, update } = useMirroredState<ProposalsState>(EMPTY);

  const status = options.status;
  const limit = options.limit;

  const refresh = useCallback(
    async (signal?: AbortSignal): Promise<void> => {
      if (chatId === null) return;
      update((prev) => ({ ...prev, loading: true }));
      try {
        const proposals = await client.listProposals(
          {
            chatId,
            ...(status === undefined ? {} : { status }),
            ...(limit === undefined ? {} : { limit }),
          },
          signal === undefined ? undefined : { signal },
        );
        if (signal?.aborted === true || !alive.current) return;
        update((prev) => ({ ...prev, proposals, loading: false, error: null }));
      } catch (cause) {
        if (isAbort(cause, signal) || !alive.current) return;
        update((prev) => ({ ...prev, loading: false, error: toError(cause) }));
      }
    },
    [client, chatId, status, limit, update, alive],
  );

  /** Run one decision/apply, then re-read: a decision changes more than one row. */
  const act = useCallback(
    async (call: () => Promise<ProposalDto>): Promise<ProposalDto | null> => {
      update((prev) => ({ ...prev, busy: true, error: null }));
      try {
        const proposal = await call();
        if (!alive.current) return proposal;
        update((prev) => ({ ...prev, busy: false }));
        await refresh();
        return proposal;
      } catch (cause) {
        if (!alive.current) return null;
        update((prev) => ({ ...prev, busy: false, error: toError(cause) }));
        return null;
      }
    },
    [update, alive, refresh],
  );

  const approve = useCallback<UseProposalsResult["approve"]>(
    (proposalId, reason) =>
      act(() =>
        client.approveProposal(
          { proposalId },
          reason === undefined ? {} : { reason },
        ),
      ),
    [act, client],
  );

  const reject = useCallback<UseProposalsResult["reject"]>(
    (proposalId, reason) =>
      act(() =>
        client.rejectProposal(
          { proposalId },
          reason === undefined ? {} : { reason },
        ),
      ),
    [act, client],
  );

  const apply = useCallback<UseProposalsResult["apply"]>(
    (proposalId, operationId) =>
      act(() =>
        client.applyProposal(
          { proposalId },
          { operationId: operationId ?? newIdempotencyKey() },
        ),
      ),
    [act, client],
  );

  useEffect(() => {
    if (chatId === null) {
      update(() => EMPTY);
      return;
    }
    const controller = new AbortController();
    void refresh(controller.signal);
    return () => controller.abort();
  }, [chatId, refresh, update]);

  // A finished run may have staged one. Nothing this hook did causes that, so
  // nothing this hook did can be what triggers the re-read.
  useTopicSubscription(
    emitter,
    chatId === null ? null : chatTopic(chatId),
    () => {
      void refresh();
    },
  );

  const reload = useCallback<UseProposalsResult["reload"]>(
    () => refresh(),
    [refresh],
  );

  return { ...value, approve, reject, apply, reload };
}
