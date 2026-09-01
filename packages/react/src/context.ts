/**
 * The one component this package ships, and the only reason it ships one.
 *
 * `AgentKitProvider` carries two things down the tree: the `AgentKitClient`
 * every hook calls, and the {@link ChangeEmitter} they use to invalidate one
 * another. Both are per-provider rather than per-module, because a test that
 * mounts two providers against two servers must not have them share a bus —
 * and because module-level singletons are exactly what makes a library
 * impossible to render twice on a server.
 *
 * WHY NO JSX. This file builds its element with `createElement` instead of
 * `<Ctx.Provider>`, so the package compiles with no `jsx` setting, ships a dist
 * that imports nothing but `react` itself, and needs no JSX runtime resolution
 * in a consumer's bundler. There is exactly one element in the whole package;
 * the syntax is not worth a build-configuration requirement imposed on every
 * consumer.
 */
import type { AgentKitClient } from "@agentkit/client";
import {
  createContext,
  createElement,
  useContext,
  useMemo,
  useState,
  type ReactElement,
  type ReactNode,
} from "react";
import { createChangeEmitter, type ChangeEmitter } from "./emitter.js";

/** What the provider puts on the context. */
export interface AgentKitContextValue {
  client: AgentKitClient;
  emitter: ChangeEmitter;
}

const AgentKitContext = createContext<AgentKitContextValue | null>(null);

export interface AgentKitProviderProps {
  /** The client every hook under this provider calls, unless it is handed another. */
  client: AgentKitClient;
  children?: ReactNode;
}

export function AgentKitProvider(props: AgentKitProviderProps): ReactElement {
  // `useState` with an initializer rather than `useMemo`: React may drop a
  // memo's cache under memory pressure, and re-creating the emitter would
  // silently orphan every subscription taken out against the old one.
  const [emitter] = useState(createChangeEmitter);
  const value = useMemo<AgentKitContextValue>(
    () => ({ client: props.client, emitter }),
    [props.client, emitter],
  );
  return createElement(
    AgentKitContext.Provider,
    { value },
    props.children ?? null,
  );
}

/**
 * The provider's value, or a thrown explanation.
 *
 * The throw is deliberate and is not softened to a `null` return: a hook that
 * quietly did nothing outside the provider would present as "the chat never
 * loads", which is the single most expensive way to learn about a missing
 * provider.
 */
export function useAgentKitContext(): AgentKitContextValue {
  const value = useContext(AgentKitContext);
  if (value === null) {
    // The message deliberately does not START with `@agentkit/`: a string
    // literal opening with that scope is indistinguishable from an unrewritten
    // module specifier to the umbrella build's residual-specifier guard
    // (`scripts/umbrella-specifiers.mjs`), which fails the build on one.
    throw new Error(
      "AgentKit hooks: no AgentKitProvider above this hook. Wrap the tree in " +
        "<AgentKitProvider client={createAgentKitClient({ baseUrl })}> — the " +
        "provider carries both the client and the cross-hook invalidation bus, " +
        "so an override passed to a single hook cannot stand in for it.",
    );
  }
  return value;
}

/**
 * The client in scope, or `override` when a caller supplied one.
 *
 * The override exists for the case a provider cannot cover: one component
 * talking to a SECOND host (a cloud instance beside the local one) while the
 * rest of the tree talks to the first. It replaces the client, never the
 * provider — the emitter still comes from the tree, so invalidation still
 * reaches the other hooks.
 */
export function useAgentKitClient(override?: AgentKitClient): AgentKitClient {
  const { client } = useAgentKitContext();
  return override ?? client;
}

/** The invalidation bus in scope. Exposed so an app can invalidate a chat itself. */
export function useAgentKitEmitter(): ChangeEmitter {
  return useAgentKitContext().emitter;
}
