import type * as Generated from "../generated/index.js";

export interface RawRunHandle {
  next(): Promise<string>;
  cancel(): void;
  close(): void;
  failBudget(code: string, message: string, providerRequests: number): void;
}

type Binding = typeof Generated;
let loaded: Binding | undefined;
let loading: Promise<Binding> | undefined;

export async function loadBinding(): Promise<Binding> {
  loading ??= import("../generated/index.js")
    .then((module) => {
      if (module.bindingSmoke() !== "agentkit-native")
        throw new Error("unexpected native binding identity");
      loaded = module;
      return loaded;
    })
    .catch((error: unknown) => {
      loading = undefined;
      throw new Error(
        `the native binding failed to load: ${error instanceof Error ? error.message : String(error)}`,
      );
    });
  return loading;
}

export function loadedBinding(): Binding {
  if (loaded === undefined) throw new Error("the native binding is not loaded");
  return loaded;
}

/** Raw streaming bridge. This waits for binding load before creating a run. */
export const NativeRunHandle = {
  async start(
    optionsJson: string,
    callback: (error: Error | null, payload: string) => void,
  ): Promise<RawRunHandle> {
    return (await loadBinding()).NativeRunHandle.start(optionsJson, callback);
  },
};

export async function pushRunProviderEvent(
  callId: string,
  eventJson: string,
): Promise<boolean> {
  return (await loadBinding()).pushRunProviderEvent(callId, eventJson);
}

export function resolveCall(
  callId: string,
  payload: string | null,
  error: string | null,
): boolean {
  return loadedBinding().resolveCall(callId, payload, error);
}

export function endRunProvider(callId: string): boolean {
  return loadedBinding().endRunProvider(callId);
}

export function isRunCallbackActive(callId: string): boolean {
  return loadedBinding().isRunCallbackActive(callId);
}

export async function reserveRunProviderRequest(callId: string): Promise<void> {
  await (await loadBinding()).reserveRunProviderRequest(callId);
}
