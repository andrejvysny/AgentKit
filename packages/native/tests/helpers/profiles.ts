import type { NativeAgentProfile } from "../../src/index.js";

/**
 * A host-owned profile for a binding test.
 *
 * Admission resolves `createSession`'s `profileId` against the service's
 * `ProfileRegistry` (the built-ins plus whatever `createService` was given as
 * `options.profiles`) and freezes the result into the session descriptor, so
 * the profile IS the turn's policy: `tools` is the exact allow-list the run
 * may stage — JS-registered tools included, since the host filters every
 * staged tool by name — `permissionMode` seeds the durable execution state
 * the JS-tool guard consults, and `limits` cap the tool loop.
 *
 * Each test therefore declares the policy its own scenario exercises and
 * nothing broader: a scenario that waits for a human says `ask`/`plan`, a
 * scenario that only needs a tool to run says `full_auto`, and a scenario
 * that stages no tools at all leaves `tools` empty.
 */
export function testProfile(
  id: string,
  spec: {
    /** Exactly the tool names the scenario stages. */
    tools?: readonly string[];
    /** Initial durable execution mode. Default `full_auto`. */
    permissionMode?: NativeAgentProfile["permissionMode"];
    kind?: NativeAgentProfile["kind"];
    role?: NativeAgentProfile["role"];
    instructions?: string;
    maxToolIterations?: number;
    maxToolCallsPerIteration?: number;
    historyLimit?: number;
    projectInstructions?: boolean;
  } = {},
): NativeAgentProfile {
  const limits: NonNullable<NativeAgentProfile["limits"]> = {
    maxToolIterations: spec.maxToolIterations ?? 30,
  };
  if (spec.maxToolCallsPerIteration !== undefined) {
    limits.maxToolCallsPerIteration = spec.maxToolCallsPerIteration;
  }
  if (spec.historyLimit !== undefined) {
    limits.historyLimit = spec.historyLimit;
  }
  const profile: NativeAgentProfile = {
    id,
    kind: spec.kind ?? "automation",
    instructions: spec.instructions ?? "",
    tools: [...(spec.tools ?? [])],
    permissionMode: spec.permissionMode ?? "full_auto",
    limits,
  };
  if (spec.role !== undefined) profile.role = spec.role;
  if (spec.projectInstructions !== undefined) {
    profile.resources = { projectInstructions: spec.projectInstructions };
  }
  return profile;
}
