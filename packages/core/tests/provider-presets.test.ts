import { describe, expect, it } from "bun:test";
import {
  AI_PROVIDER_PRESETS,
  getPresetByKind,
  listPresets,
} from "../src/providers/presets.js";
import { KNOWN_PROVIDER_KINDS } from "@agentkit/contracts";

describe("provider presets", () => {
  it("exposes all 5 kinds", () => {
    const kinds = AI_PROVIDER_PRESETS.map((p) => p.kind).sort();
    expect(kinds).toEqual([
      "lmstudio",
      "omlx",
      "openai",
      "openai-compatible",
      "openrouter",
    ]);
  });

  it("covers exactly the KNOWN_PROVIDER_KINDS vocabulary", () => {
    expect([...AI_PROVIDER_PRESETS.map((p) => p.kind)].sort()).toEqual(
      [...KNOWN_PROVIDER_KINDS].sort(),
    );
  });

  it("openai requires API key", () => {
    expect(getPresetByKind("openai")?.requiresApiKey).toBe(true);
  });

  it("lmstudio and omlx do not require API key", () => {
    expect(getPresetByKind("lmstudio")?.requiresApiKey).toBe(false);
    expect(getPresetByKind("omlx")?.requiresApiKey).toBe(false);
  });

  it("returns undefined for a kind with no preset", () => {
    // The kind vocabulary is open, so an unknown kind is a legal config — it
    // simply has no shipped defaults.
    expect(getPresetByKind("some-host-gateway")).toBeUndefined();
  });

  it("omlx has empty defaults but provides probe URLs", () => {
    const omlx = getPresetByKind("omlx");
    expect(omlx?.defaultBaseUrl).toBe("");
    expect(omlx?.defaultModel).toBe("");
    expect(omlx?.probeBaseUrls?.length ?? 0).toBeGreaterThan(0);
  });

  it("carries no product-specific branding in its notes", () => {
    for (const preset of AI_PROVIDER_PRESETS) {
      expect(preset.notes ?? "").not.toMatch(/openpcb/i);
      expect(preset.label).not.toMatch(/openpcb/i);
    }
  });

  it("listPresets returns a copy", () => {
    const a = listPresets();
    const b = listPresets();
    expect(a).not.toBe(b);
    expect(a).toEqual(b);
  });
});
