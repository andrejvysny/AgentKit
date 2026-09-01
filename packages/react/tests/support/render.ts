/**
 * The two lines every test in this package would otherwise repeat: a provider
 * wrapper, and one `<StrictMode>` variant of it.
 *
 * Written with `createElement` rather than JSX for the reason `src/context.ts`
 * gives — the package compiles with no JSX runtime requirement, and a test
 * suite that needed one would be proving something the package does not do.
 */
import type { AgentKitClient } from "@agentkit/client";
import { StrictMode, createElement, type ReactNode } from "react";
import { AgentKitProvider } from "../../src/index.js";

export type Wrapper = (props: { children?: ReactNode }) => ReactNode;

/** `<AgentKitProvider client={client}>{children}</AgentKitProvider>` */
export function wrapper(client: AgentKitClient): Wrapper {
  return ({ children }) =>
    createElement(AgentKitProvider, { client }, children ?? null);
}

/**
 * The same, inside `<StrictMode>`.
 *
 * React 18+ double-invokes every effect on mount in development — run, tear
 * down, run again — which is the cheapest available test of "does this hook
 * clean up after itself". A subscription that leaked or a stream that was not
 * aborted shows up here as a duplicate, never anywhere else.
 */
export function strictWrapper(client: AgentKitClient): Wrapper {
  return ({ children }) =>
    createElement(
      StrictMode,
      null,
      createElement(AgentKitProvider, { client }, children ?? null),
    );
}

/** Poll until `predicate` holds, or fail loudly about what never happened. */
export async function until(
  predicate: () => boolean | Promise<boolean>,
  what: string,
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`Timed out waiting for ${what}.`);
}
