/**
 * The invalidation bus every hook in this package shares — a `Map` of `Set`s
 * and nothing else.
 *
 * WHY THIS EXISTS. The hooks are independent by design: `useBranches` knows
 * about siblings, `useChat` knows about a message list, `useProposals` knows
 * about a review queue, and none of them holds a reference to the others. But
 * the server's state is NOT independent — activating a branch changes which
 * messages `listMessages` reports, and a run reaching its terminal event can
 * mint a proposal. A hook that did not hear about those would render a
 * conversation the server stopped serving until something else happened to
 * re-render it.
 *
 * WHY NOT A QUERY CACHE. The obvious answer is TanStack Query, and it is the
 * right answer for an application — it is the wrong answer for a library that
 * must not choose the application's data layer for it. A peer dependency on a
 * cache is a peer dependency on its version, its `QueryClientProvider`, its
 * devtools and its opinions about retries; what this package actually needs is
 * "tell me when this chat changed", which is thirty lines. An application that
 * wants a real cache wraps these hooks or ignores them and calls
 * `@agentkit/client` directly.
 *
 * WHAT IT IS NOT: a state container. Nothing is stored here, no payload travels
 * on a topic, and a subscriber's only correct reaction is to re-read from the
 * server. That is deliberate — two hooks that agreed on a cached value would be
 * a second source of truth, and the server already is the first one.
 */

/** Everything a listener is told about the change. */
export interface ChangeEvent {
  /**
   * The hook instance that caused the change, when one did.
   *
   * A hook that emits also SUBSCRIBES to the same topic, and the point of this
   * field is that it can tell its own echo apart from somebody else's news: a
   * `useChat` that reloaded the message list as the last step of a submit does
   * not need to reload it again because it announced the submit.
   */
  origin?: string;
}

export type ChangeListener = (event: ChangeEvent) => void;

export interface ChangeEmitter {
  /** Announce that `topic` is stale. Listener throws are swallowed, not propagated. */
  emit(topic: string, event?: ChangeEvent): void;
  /** Listen; the returned function unsubscribes and is safe to call twice. */
  subscribe(topic: string, listener: ChangeListener): () => void;
}

/** The topic a chat's message list, branches and proposals all hang off. */
export function chatTopic(chatId: string): string {
  return `chat:${chatId}:changed`;
}

export function createChangeEmitter(): ChangeEmitter {
  const topics = new Map<string, Set<ChangeListener>>();

  return {
    emit(topic, event = {}) {
      const listeners = topics.get(topic);
      if (listeners === undefined) return;
      // Copied before iterating: a listener that unsubscribes itself while the
      // set is being walked would otherwise mutate what is being iterated.
      for (const listener of [...listeners]) {
        try {
          listener(event);
        } catch {
          // A subscriber that throws is that subscriber's bug. Letting it
          // escape here would take out every OTHER subscriber of the same
          // topic, which turns one broken component into a stale application.
        }
      }
    },

    subscribe(topic, listener) {
      let listeners = topics.get(topic);
      if (listeners === undefined) {
        listeners = new Set();
        topics.set(topic, listeners);
      }
      listeners.add(listener);
      let live = true;
      return () => {
        if (!live) return;
        live = false;
        listeners.delete(listener);
        if (listeners.size === 0) topics.delete(topic);
      };
    },
  };
}

/** A process-unique id for a hook instance, used as {@link ChangeEvent.origin}. */
let originCounter = 0;
export function nextOrigin(prefix: string): string {
  originCounter += 1;
  return `${prefix}-${originCounter}`;
}
