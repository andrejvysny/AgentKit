import type { AiContextSizePreference } from "@agentkit/contracts";
import type { WritePolicyMode } from "./write-policy.js";

/**
 * Assistant-wide settings. One row: these are the knobs a host exposes in a
 * settings pane, not per-chat state.
 */
export interface AssistantSettings {
  defaultProviderId?: string;
  defaultModel?: string;
  /** Drives `resolveToolLimits` — how much tool output may enter the context. */
  contextSizePreference: AiContextSizePreference;
  /** Default confirmation posture for writes; see {@link WritePolicyMode}. */
  writePolicyMode: WritePolicyMode;
  /** Expose raw tool payloads to the model instead of slim envelopes. */
  allowRawToolData: boolean;
  maxToolIterations?: number;
  /**
   * Manual override for whether a turn is handed tools at all. Default `auto`.
   *
   * - `auto` — follow the provider's probed capabilities: a provider recorded as
   *   `toolCalling: false` gets no tools, because handing them over is the exact
   *   request shape that fails.
   * - `on` — stage tools even when the probe says unsupported. This is the whole
   *   reason the knob exists: probing is a heuristic against someone else's
   *   server, and a wrong `false` otherwise leaves a perfectly capable model
   *   permanently toolless with no way to say so.
   * - `off` — stage no tools at all, whatever the provider can do. A chat-only
   *   mode, and the cheapest way to take a misbehaving tool loop out of the
   *   picture without unwiring contributors.
   */
  toolCalling?: ToolCallingMode;
  metadata: Record<string, unknown>;
}

/** See {@link AssistantSettings.toolCalling}. */
export type ToolCallingMode = "auto" | "on" | "off";

export interface SettingsStore {
  getSettings(): Promise<AssistantSettings>;
  /** Partial update; returns the settings as they now stand. */
  updateSettings(patch: Partial<AssistantSettings>): Promise<AssistantSettings>;
}
