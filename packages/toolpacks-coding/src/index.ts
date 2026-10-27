// Public barrel for @agentkit/toolpacks-coding: an OPTIONAL tool pack that
// gives an agent confined filesystem read/search/edit tools, read-only git
// inspection and sandboxed command/test execution, plus the guard that
// decides which of them a run may use. Node APIs only; macOS-first (the
// sandbox is `sandbox-exec`). See docs/adr/0018-coding-tool-pack.md.

export { createCodingToolPack, CODING_TOOL_NAMES } from "./pack.js";
export {
  createCodingPermissionGuard,
  CODING_NAMESPACE,
  PLAN_MODE_BLOCKED_HINT,
  type CodingPermissionGuardOptions,
} from "./guard.js";
export {
  SUBMIT_PLAN_TOOL_NAME,
  SUBMIT_PLAN_CAPABILITY,
  submitPlanTool,
} from "./plan-tool.js";
export { DEFAULT_CODING_LIMITS, type CodingLimits } from "./limits.js";
export type {
  CodingMode,
  CodingPermissionAuthority,
  CodingRole,
  CodingToolPackOptions,
  CodingWorkspace,
} from "./types.js";
