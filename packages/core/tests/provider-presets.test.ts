import { describe, expect, it } from "bun:test";
import {
  AI_PROVIDER_PRESETS,
  getPresetByKind,
  listPresets,
} from "../src/providers/presets.js";

describe("provider presets", () => {
  it("exposes all 6 kinds", () => {
    const kinds = AI_PROVIDER_PRESETS.map((p) => p.kind).sort();
    expect(kinds).toEqual([
      "lmstudio",
      "omlx",
      "openai",
      "openai-compatible",
      "openpcb-cloud",
      "openrouter",
    ]);
  });

  it("openai requires API key", () => {
    expect(getPresetByKind("openai")?.requiresApiKey).toBe(true);
  });

  it("lmstudio and omlx do not require API key", () => {
    expect(getPresetByKind("lmstudio")?.requiresApiKey).toBe(false);
    expect(getPresetByKind("omlx")?.requiresApiKey).toBe(false);
  });

  it("openpcb-cloud does not require API key (bearer sealed per run)", () => {
    const cloud = getPresetByKind("openpcb-cloud");
    expect(cloud?.requiresApiKey).toBe(false);
    expect(cloud?.defaultBaseUrl).toBe("");
    expect(cloud?.defaultModel).toBe("");
  });

  it("omlx has empty defaults but provides probe URLs", () => {
    const omlx = getPresetByKind("omlx");
    expect(omlx?.defaultBaseUrl).toBe("");
    expect(omlx?.defaultModel).toBe("");
    expect(omlx?.probeBaseUrls?.length ?? 0).toBeGreaterThan(0);
  });

  it("listPresets returns a copy", () => {
    const a = listPresets();
    const b = listPresets();
    expect(a).not.toBe(b);
    expect(a).toEqual(b);
  });
});
