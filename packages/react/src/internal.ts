/**
 * The three pieces of plumbing every hook here repeats, written once.
 *
 * All of them exist because these hooks are ASYNCHRONOUS against a server that
 * streams: a submit starts work that outlives the click, a run stream outlives
 * several renders, and any of it can still be in flight when the component
 * unmounts. React's rules for that are unforgiving in exactly two ways — a
 * `setState` after unmount is a leak the user sees as an act() warning, and a
 * state update computed from a closed-over render's value is a lost update —
 * and both are avoided by the same two objects: a liveness ref and a state
 * mirror.
 */
import { AgentKitClientError } from "@agentkit/client";
import { useCallback, useEffect, useRef, useState } from "react";
import type { ChangeEmitter, ChangeListener } from "./emitter.js";

/**
 * `true` between mount and unmount, readable from an async callback.
 *
 * Initialised `true` rather than `false`: an action can fire before the mount
 * effect has run (React 18 flushes some effects late), and a guard that started
 * `false` would silently discard that action's first state update.
 *
 * Under `<StrictMode>` the effect runs, tears down and runs again; the flag
 * ends `true` either way, which is the property that matters.
 */
export function useAliveRef(): { readonly current: boolean } {
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  return alive;
}

export interface MirroredState<T> {
  /** The value this render sees. */
  value: T;
  /** The value RIGHT NOW, including updates this render has not seen yet. */
  read(): T;
  /** Apply an update against {@link read}, then re-render. No-op after unmount. */
  update(next: (prev: T) => T): void;
}

/**
 * `useState` with a ref that always holds the latest value.
 *
 * The mirror is not an optimisation. An async flow here reads its own state
 * between awaits — a stream applying a delta to the placeholder it created
 * three awaits ago, a submit that needs the pre-optimistic message list to roll
 * back to — and `useState`'s functional updater cannot answer a READ: it hands
 * the previous value to a callback that runs later, during render. Computing
 * the next value eagerly against the ref makes both directions work, and
 * because EVERY update goes through here, the ref cannot drift from the state.
 *
 * THE REF IS THE SOURCE OF TRUTH; `useState` exists only to schedule the
 * re-render. Nothing writes the ref from the render body — the tempting
 * `ref.current = value` on every render is wrong under concurrent React, where
 * a low-priority render can legitimately run with a value the ref has already
 * moved past (a `startTransition` around a `submit`, say), and copying it back
 * would erase the newer state.
 *
 * The unmount guard lives here too, so no caller has to remember it.
 */
export function useMirroredState<T>(initial: T | (() => T)): MirroredState<T> {
  const [value, setValue] = useState<T>(initial);
  const ref = useRef<T>(value);
  const alive = useAliveRef();

  const read = useCallback(() => ref.current, []);
  const update = useCallback(
    (next: (prev: T) => T) => {
      if (!alive.current) return;
      const computed = next(ref.current);
      if (Object.is(computed, ref.current)) return;
      ref.current = computed;
      setValue(computed);
    },
    [alive],
  );

  return { value, read, update };
}

/**
 * Anything a rejected promise carried, as something a component can render.
 *
 * `AgentKitClientError` passes through untouched — it is the shape with the
 * `code` a UI branches on. Everything else that is already an `Error` passes
 * through as well; only a non-`Error` rejection (a string, a `Response`, a
 * bare object) gets wrapped, because a component that has to type-guard the
 * `error` field before showing it will not.
 */
export function toError(cause: unknown): AgentKitClientError | Error {
  if (cause instanceof AgentKitClientError) return cause;
  if (cause instanceof Error) return cause;
  return new Error(typeof cause === "string" ? cause : String(cause));
}

/** True when a rejection is just "the caller aborted", which is not a failure. */
export function isAbort(cause: unknown, signal?: AbortSignal): boolean {
  if (signal?.aborted === true) return true;
  return cause instanceof Error && cause.name === "AbortError";
}

/**
 * Subscribe to a topic for the life of the component.
 *
 * `listener` is kept in a ref so a caller does not have to memoise it: a
 * subscription that re-registered on every render would unsubscribe and
 * re-subscribe on every keystroke of the surrounding form.
 */
export function useTopicSubscription(
  emitter: ChangeEmitter,
  topic: string | null,
  listener: ChangeListener,
): void {
  const latest = useRef(listener);
  latest.current = listener;

  useEffect(() => {
    if (topic === null) return;
    return emitter.subscribe(topic, (event) => {
      latest.current(event);
    });
  }, [emitter, topic]);
}
