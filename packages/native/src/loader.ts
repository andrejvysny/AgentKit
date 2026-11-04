// Pull the ambient `node:module` shape in wherever this file is compiled:
// sibling packages (core, host) reach `src/` through the workspace `types`
// export whenever `native/dist` is absent, and their tsconfigs never include
// this package's ambient declarations (TS2307 on clean CI checkouts).
/// <reference path="./node-builtins.d.ts" />
import type * as Generated from "../generated/index.js";
import { NATIVE_ADDON_VERSION } from "./addon-version.js";

/**
 * The generated NAPI-RS loader's embedded version comparison is opt-in
 * (`NAPI_RS_ENFORCE_VERSION_CHECK`), so by default a wrapper could load an
 * addon package whose version differs from its own — a silent ABI-skew
 * hazard in a partially-published or overridden install. The wrapper pins
 * every platform package to an exact version; verify what actually
 * resolved, unconditionally. The keys mirror
 * `rust/scripts/native-platform-targets.mjs` (pinned by
 * `tests/distribution.test.ts`). A package that does not resolve is not an
 * error HERE: os/cpu gating legitimately keeps foreign packages out, and
 * the addon import below turns a genuinely missing binding into the loud
 * load failure.
 *
 * Node builtins are reached only LAZILY inside this function (invoked
 * once the native binding is already loading, i.e. a Node-class runtime):
 * the published wrapper keeps zero static Node dependencies so bundlers
 * can still resolve the pure-TS import surface.
 */
async function assertAddonVersionsMatchWrapper(): Promise<void> {
  // Same documented escape hatch as the generated loader's own checks
  // (napi-rs convention): `NAPI_RS_ENFORCE_VERSION_CHECK=0` disables the
  // version comparison for debugging a deliberately patched install.
  // Anything else — unset, empty, "1", garbage — keeps it ON. `globalThis`
  // is narrowed rather than cast: this package keeps ZERO Node types.
  const global: unknown = globalThis;
  if (
    typeof global === "object" &&
    global !== null &&
    "process" in global &&
    typeof global.process === "object" &&
    global.process !== null &&
    "env" in global.process &&
    typeof global.process.env === "object" &&
    global.process.env !== null &&
    "NAPI_RS_ENFORCE_VERSION_CHECK" in global.process.env &&
    global.process.env.NAPI_RS_ENFORCE_VERSION_CHECK === "0"
  )
    return;
  const { createRequire } = await import("node:module");
  const requireLocal = createRequire(import.meta.url);
  for (const key of [
    "linux-x64-gnu",
    "linux-arm64-gnu",
    "darwin-arm64",
    "darwin-x64",
    "win32-x64-msvc",
  ]) {
    let addon: { version?: unknown } | undefined;
    try {
      addon = requireLocal(`@agentkit/native-${key}/package.json`) as {
        version?: unknown;
      };
    } catch {
      continue;
    }
    if (addon.version !== NATIVE_ADDON_VERSION)
      throw new Error(
        `native binding package version mismatch: @agentkit/native-${key} is ${String(addon.version)}, wrapper is ${NATIVE_ADDON_VERSION}`,
      );
  }
}

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
    .then(async (module) => {
      if (module.bindingSmoke() !== "agentkit-native")
        throw new Error("unexpected native binding identity");
      await assertAddonVersionsMatchWrapper();
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

/** Synchronous liveness claim for one routed callback: true exactly once,
 * only while Rust still considers the call live. The router MUST gate every
 * entry into user code on this answer — a false means the host already
 * settled the call (timeout/cancel/close) and the envelope is stale. */
export function claimCall(callId: string): boolean {
  return loadedBinding().claimCall(callId);
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
