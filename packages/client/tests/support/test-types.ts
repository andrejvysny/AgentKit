import type { AiContextBinding, AiToolLimits } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import type { ProposalRecord } from "../../../native/src/ports.js";

export type { TaskStore } from "../../../native/src/ports.js";

/** What the Rust loop hands a test-side tool contributor (type-only mirror). */
export interface ToolContributionContext {
  chatId?: string;
  runId?: string;
  scopeId?: string;
  principal?: string;
  actorId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
}

/** A test-side tool source bridged to the Rust loop by `provider-bridge.ts`. */
export interface ToolSetContributor {
  namespace: string;
  contribute(ctx: ToolContributionContext): Promise<AiTool[]>;
}

export interface StageProposalInput {
  chatId: string;
  scopeKey: string;
  actionId?: string;
  toolName: string;
  kind: string;
  risk: ProposalRecord["risk"];
  operations: unknown[];
}

/** The one proposal control the native test listener implements. */
export interface TestProposalService {
  stage(input: StageProposalInput): Promise<ProposalRecord>;
}
