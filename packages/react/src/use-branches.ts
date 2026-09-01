/**
 * `useBranches` — the "‹ 2 / 3 ›" control above a regenerated answer.
 *
 * A chat is a TREE: every regeneration and every edited question adds a sibling
 * under the same parent, and exactly one path through that tree is active at a
 * time. `listMessages` reports the active path and nothing else, which is why
 * switching branches is a server write (`activateBranch`) rather than a local
 * selection — the next turn replays the path the SERVER thinks is active, and a
 * client that had only changed its own mind would send the model a
 * conversation nobody was looking at.
 *
 * THAT IS WHY THIS HOOK EMITS. `activate` invalidates the chat topic with no
 * `origin`, so every `useChat` on that chat re-reads its message list; without
 * it, switching a branch would change the little counter and nothing else.
 */
import type { AgentKitClient, AgentKitClientError } from "@agentkit/client";
import type { MessageDto } from "@agentkit/contracts";
import { useCallback, useEffect, useRef } from "react";
import { useAgentKitClient, useAgentKitContext } from "./context.js";
import { chatTopic, nextOrigin } from "./emitter.js";
import {
  isAbort,
  toError,
  useAliveRef,
  useMirroredState,
  useTopicSubscription,
} from "./internal.js";

export interface BranchState {
  /** Siblings sharing a parent, INCLUDING the message asked about, `branchIndex` ascending. */
  siblings: MessageDto[];
  /**
   * Position of the ACTIVE sibling in {@link siblings}, or `-1` while nothing
   * is loaded. The active one — not the one this hook was asked about: the
   * counter a user reads says which branch they are on.
   */
  index: number;
  count: number;
  loading: boolean;
  error: AgentKitClientError | Error | null;
}

export interface UseBranchesOptions {
  client?: AgentKitClient;
}

export interface UseBranchesResult extends BranchState {
  /** Make `siblingId`'s branch the active path, then invalidate the chat. */
  activate(siblingId: string): Promise<void>;
  reload(): Promise<void>;
}

const EMPTY: BranchState = {
  siblings: [],
  index: -1,
  count: 0,
  loading: false,
  error: null,
};

export function useBranches(
  messageId: string | null,
  options: UseBranchesOptions = {},
): UseBranchesResult {
  const { emitter } = useAgentKitContext();
  const client = useAgentKitClient(options.client);
  const alive = useAliveRef();
  const { value, read, update } = useMirroredState<BranchState>(EMPTY);

  const originRef = useRef<string | null>(null);
  if (originRef.current === null) originRef.current = nextOrigin("branches");
  const origin = originRef.current;

  const refresh = useCallback(
    async (signal?: AbortSignal): Promise<void> => {
      if (messageId === null) return;
      update((prev) => ({ ...prev, loading: true }));
      try {
        const siblings = await client.listSiblings(
          { messageId },
          signal === undefined ? undefined : { signal },
        );
        if (signal?.aborted === true || !alive.current) return;
        update(() => ({
          siblings,
          index: activeIndex(siblings, messageId),
          count: siblings.length,
          loading: false,
          error: null,
        }));
      } catch (cause) {
        if (isAbort(cause, signal) || !alive.current) return;
        update((prev) => ({
          ...prev,
          loading: false,
          error: toError(cause),
        }));
      }
    },
    [client, messageId, update, alive],
  );

  const activate = useCallback<UseBranchesResult["activate"]>(
    async (siblingId) => {
      update((prev) => ({ ...prev, loading: true, error: null }));
      try {
        // The switch answers with the path that became active, in one round
        // trip — a follow-up `listMessages` could already be a turn behind.
        const path = await client.activateBranch({ messageId: siblingId });
        if (!alive.current) return;
        const chatId =
          path.items[0]?.chatId ?? read().siblings[0]?.chatId ?? null;
        await refresh();
        // Every `useChat` on this chat is now holding the wrong path. The
        // `origin` is this hook's, so the announcement reaches them and not the
        // subscription below — which has just re-read.
        if (chatId !== null) emitter.emit(chatTopic(chatId), { origin });
      } catch (cause) {
        if (!alive.current) return;
        update((prev) => ({
          ...prev,
          loading: false,
          error: toError(cause),
        }));
      }
    },
    [client, read, update, alive, emitter, origin, refresh],
  );

  // A regeneration adds a sibling, and it is `useChat` that asks for one. The
  // chat id comes off the loaded siblings, so this only listens once there is
  // something to be stale — which is exactly when a count can change.
  useTopicSubscription(
    emitter,
    value.siblings[0]?.chatId === undefined
      ? null
      : chatTopic(value.siblings[0].chatId),
    (event) => {
      if (event.origin === origin) return;
      void refresh();
    },
  );

  useEffect(() => {
    if (messageId === null) {
      update(() => EMPTY);
      return;
    }
    const controller = new AbortController();
    void refresh(controller.signal);
    return () => controller.abort();
  }, [messageId, refresh, update]);

  const reload = useCallback<UseBranchesResult["reload"]>(
    () => refresh(),
    [refresh],
  );

  return { ...value, activate, reload };
}

/**
 * Which sibling is on the active path.
 *
 * `active` is OPTIONAL in the contract — a pre-branching server omits it — so a
 * list with no active flag falls back to the message that was asked about,
 * which on such a server is the only one there is.
 */
function activeIndex(
  siblings: readonly MessageDto[],
  messageId: string,
): number {
  const flagged = siblings.findIndex((message) => message.active === true);
  if (flagged !== -1) return flagged;
  return siblings.findIndex((message) => message.id === messageId);
}
