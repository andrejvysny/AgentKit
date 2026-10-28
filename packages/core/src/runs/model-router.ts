import type { AiProviderModel } from "@agentkit/contracts";
import type { AiProviderClient } from "../providers/client.js";

/**
 * Capability-aware model routing (Phase B / B3). Routing is OPTIONAL: a run
 * with no `modelRouter` takes the exact pre-B3 code path. The router is a
 * host-supplied selector consulted before each provider request; the loop
 * still owns the run — one loop, one orchestration owner, one budget.
 *
 * Hard rules enforced here, not left to router implementations:
 * - a selection must be a well-formed `{providerId, modelId}`;
 * - the host must authorize every selection (policy ceilings, capability
 *   validation, credential scope) before the request is dispatched;
 * - a cross-provider switch needs an explicit `resolveClient` — there is no
 *   silent remote fallback to the run's original provider;
 * - provider continuation state is never carried across a model switch
 *   unless the host explicitly chose the `reset` policy.
 */

/** Identifies one model on one provider. Plain data; safe to log. */
export interface ModelRef {
  providerId: string;
  modelId: string;
}

/** What the router may see when asked to choose. Never carries credentials. */
export interface ModelSelectionContext {
  runId: string;
  /** 1-based iteration of the loop for the step about to dispatch. */
  step: number;
  /** The model used for the previous step of this run, if any. */
  previousModel?: ModelRef;
  /** Capabilities the upcoming step needs; an empty list means "any model". */
  requiredCapabilities: readonly string[];
  signal?: AbortSignal;
}

export interface ModelRouter {
  selectModel(context: ModelSelectionContext): ModelRef | Promise<ModelRef>;
}

export type ModelRoutingErrorCode =
  | "invalid_selection"
  | "provider_unresolved"
  | "continuation_conflict"
  | "no_capable_model"
  | "authorization_denied";

export class ModelRoutingError extends Error {
  constructor(
    public readonly code: ModelRoutingErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "ModelRoutingError";
  }
}

export interface StepModelSelectionParams {
  router: ModelRouter;
  /** The model in force for the previous step (initially the run's model). */
  current: ModelRef;
  runId: string;
  step: number;
  requiredCapabilities: readonly string[];
  signal?: AbortSignal;
  /** Whether opaque provider continuation state exists right now. */
  hasContinuation: boolean;
  /** What happens when a switch would invalidate that continuation. */
  continuationSwitchPolicy: "reject" | "reset";
  /**
   * Host authorization: MUST throw to veto a selection (role ceiling, high-risk
   * model guard, credential-scope check, capability-metadata validation).
   */
  authorize?: (
    selection: ModelRef,
    context: ModelSelectionContext,
  ) => void | Promise<void>;
}

/**
 * Ask the router for this step's model, then enforce the invariants. Returns
 * the selection, or `undefined` when the router explicitly kept the current
 * model and no check fires (the common no-op case, which the loop uses to
 * skip making a step-scoped copy of the input).
 */
export async function selectStepModel(
  params: StepModelSelectionParams,
): Promise<ModelRef | undefined> {
  const context: ModelSelectionContext = {
    runId: params.runId,
    step: params.step,
    ...(params.current ? { previousModel: params.current } : {}),
    requiredCapabilities: params.requiredCapabilities,
    signal: params.signal,
  };
  const selection = await params.router.selectModel(context);
  if (
    selection === null ||
    typeof selection !== "object" ||
    typeof selection.providerId !== "string" ||
    selection.providerId.length === 0 ||
    typeof selection.modelId !== "string" ||
    selection.modelId.length === 0
  ) {
    throw new ModelRoutingError(
      "invalid_selection",
      "model router returned a malformed ModelRef",
    );
  }
  const switched =
    selection.providerId !== params.current.providerId ||
    selection.modelId !== params.current.modelId;
  if (switched && params.hasContinuation) {
    if (params.continuationSwitchPolicy === "reject") {
      throw new ModelRoutingError(
        "continuation_conflict",
        `model router switched ${params.current.providerId}/${params.current.modelId} -> ${selection.providerId}/${selection.modelId} while provider continuation state exists; use continuationModelSwitch: "reset" to authorize discarding it`,
      );
    }
  }
  try {
    await params.authorize?.(selection, context);
  } catch (err) {
    if (err instanceof ModelRoutingError) throw err;
    throw new ModelRoutingError(
      "authorization_denied",
      `host refused model ${selection.providerId}/${selection.modelId}: ${
        err instanceof Error ? err.message : String(err)
      }`,
    );
  }
  return switched ? selection : undefined;
}

/**
 * Resolve the client for a routed selection. Same provider as the run's own
 * client: reuse it (one identity, one credential scope). Cross-provider:
 * require `resolveClient`; never fall back silently.
 */
export async function resolveStepClient(
  baseClient: AiProviderClient,
  selection: ModelRef,
  resolveClient?: (
    providerId: string,
  ) => AiProviderClient | Promise<AiProviderClient>,
): Promise<AiProviderClient> {
  if (selection.providerId === baseClient.id) return baseClient;
  if (!resolveClient) {
    throw new ModelRoutingError(
      "provider_unresolved",
      `model router selected provider ${selection.providerId} but the run supplied no resolveClient; refusing to dispatch on an unauthorized provider`,
    );
  }
  const client = await resolveClient(selection.providerId);
  if (
    client === null ||
    typeof client !== "object" ||
    client.id !== selection.providerId ||
    typeof client.streamChat !== "function"
  ) {
    throw new ModelRoutingError(
      "provider_unresolved",
      `resolveClient(${selection.providerId}) did not return a client for that provider`,
    );
  }
  return client;
}

/** Capability name → the model metadata field that must be `true`. */
const CAPABILITY_FIELDS: Record<
  string,
  (m: AiProviderModel) => boolean | undefined
> = {
  streaming: (m) => m.supportsStreaming,
  toolCalling: (m) => m.supportsToolCalling,
  "tool-calling": (m) => m.supportsToolCalling,
  json: (m) => m.supportsJsonMode,
  "json-mode": (m) => m.supportsJsonMode,
  vision: (m) => m.supportsVision,
  audio: (m) => m.supportsAudio,
  reasoning: (m) => m.supportsReasoning,
};

export interface CapabilityAwareRouterOptions {
  /**
   * Candidate catalogue, consulted per selection. Typically
   * `() => store.providers.listModels(providerId)` or the merged result of
   * `mergeModelMetadata`.
   */
  models: () => AiProviderModel[] | Promise<AiProviderModel[]>;
  /** Restrict candidates to one provider. Omit to route across providers. */
  providerId?: string;
  /**
   * Optional ranking hook (cost/latency aware selection): higher wins; ties
   * keep catalogue order. Never consulted for models that fail the
   * capability requirements.
   */
  prefer?: (model: AiProviderModel, context: ModelSelectionContext) => number;
}

/**
 * Built-in capability-aware router. Picks a candidate whose declared
 * metadata proves every required capability; absent/undefined facts do NOT
 * count as satisfied (unknown stays unknown — no fabrication). The previous
 * model is sticky while it still qualifies, so a run does not thrash between
 * equivalent models. When nothing qualifies the run fails closed with
 * `no_capable_model`; there is no silent fallback to an incapable model.
 */
export function createCapabilityAwareRouter(
  options: CapabilityAwareRouterOptions,
): ModelRouter {
  return {
    async selectModel(context) {
      const all = await options.models();
      const pool = options.providerId
        ? all.filter((m) => m.providerId === options.providerId)
        : all;
      const qualifies = (m: AiProviderModel): boolean =>
        context.requiredCapabilities.every((cap) => {
          const field = CAPABILITY_FIELDS[cap];
          return field !== undefined && field(m) === true;
        });
      if (context.previousModel) {
        const previous = pool.find(
          (m) =>
            m.providerId === context.previousModel?.providerId &&
            m.modelId === context.previousModel?.modelId,
        );
        if (previous && qualifies(previous)) {
          return {
            providerId: previous.providerId,
            modelId: previous.modelId,
          };
        }
      }
      let best: AiProviderModel | undefined;
      let bestScore = Number.NEGATIVE_INFINITY;
      for (const m of pool) {
        if (!qualifies(m)) continue;
        const score = options.prefer ? options.prefer(m, context) : 0;
        if (score > bestScore) {
          best = m;
          bestScore = score;
        }
      }
      if (!best) {
        throw new ModelRoutingError(
          "no_capable_model",
          `no configured model declares all required capabilities [${context.requiredCapabilities.join(", ")}]`,
        );
      }
      return { providerId: best.providerId, modelId: best.modelId };
    },
  };
}
