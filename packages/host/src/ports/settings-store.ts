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
  metadata: Record<string, unknown>;
}

export interface SettingsStore {
  getSettings(): Promise<AssistantSettings>;
  /** Partial update; returns the settings as they now stand. */
  updateSettings(patch: Partial<AssistantSettings>): Promise<AssistantSettings>;
}
