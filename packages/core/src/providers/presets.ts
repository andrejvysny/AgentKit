import type { AiProviderKind } from "./types.js";

export interface AiProviderPreset {
  kind: AiProviderKind;
  label: string;
  defaultBaseUrl: string;
  defaultModel: string;
  requiresApiKey: boolean;
  docsUrl?: string;
  notes?: string;
  /**
   * Candidate base URLs to probe when auto-detecting a local provider.
   * The first one that responds to GET /models is treated as alive.
   */
  probeBaseUrls?: string[];
}

export const AI_PROVIDER_PRESETS: readonly AiProviderPreset[] = Object.freeze([
  {
    kind: "openai",
    label: "OpenAI",
    defaultBaseUrl: "https://api.openai.com/v1",
    defaultModel: "gpt-4o-mini",
    requiresApiKey: true,
    docsUrl: "https://platform.openai.com/docs/api-reference",
    notes: "Cloud provider. Requires an API key.",
  },
  {
    kind: "openrouter",
    label: "OpenRouter",
    defaultBaseUrl: "https://openrouter.ai/api/v1",
    defaultModel: "anthropic/claude-3.5-sonnet",
    requiresApiKey: true,
    docsUrl: "https://openrouter.ai/docs",
    notes:
      "Cloud aggregator routing to many model providers via one OpenAI-compatible API. Requires an API key from openrouter.ai/keys. Models use vendor/model slugs (e.g. anthropic/claude-3.5-sonnet).",
  },
  {
    kind: "openpcb-cloud",
    label: "OpenPCB Cloud",
    defaultBaseUrl: "",
    defaultModel: "",
    requiresApiKey: false,
    notes:
      "Zero-config cloud AI for Pro subscribers. Auto-configured after you sign in — no API key or base URL needed. The per-run bearer is your live session token.",
  },
  {
    kind: "lmstudio",
    label: "LM Studio",
    defaultBaseUrl: "http://127.0.0.1:1234/v1",
    defaultModel: "local-model",
    requiresApiKey: false,
    docsUrl: "https://lmstudio.ai/docs/local-server",
    notes:
      "Run LM Studio locally and start its OpenAI-compatible server. Tool calling depends on the loaded model.",
    probeBaseUrls: ["http://127.0.0.1:1234/v1"],
  },
  {
    kind: "omlx",
    label: "oMLX",
    defaultBaseUrl: "",
    defaultModel: "",
    requiresApiKey: false,
    docsUrl: "https://github.com/ml-explore/mlx",
    notes:
      "Apple-Silicon-optimized local provider. Start the oMLX server then fill in the base URL and model below.",
    probeBaseUrls: [
      "http://127.0.0.1:8080/v1",
      "http://127.0.0.1:8000/v1",
      "http://127.0.0.1:11434/v1",
    ],
  },
  {
    kind: "openai-compatible",
    label: "Custom OpenAI-compatible",
    defaultBaseUrl: "http://127.0.0.1:8000/v1",
    defaultModel: "local-model",
    requiresApiKey: false,
    notes:
      "Generic OpenAI-compatible endpoint (vLLM, llama.cpp, Ollama, etc.).",
  },
]);

export function getPresetByKind(
  kind: AiProviderKind,
): AiProviderPreset | undefined {
  return AI_PROVIDER_PRESETS.find((p) => p.kind === kind);
}

export function listPresets(): AiProviderPreset[] {
  return [...AI_PROVIDER_PRESETS];
}
