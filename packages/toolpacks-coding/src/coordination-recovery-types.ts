import type { ExecutionCleanup } from "./coordination.js";

export type WorkspaceRecoveryReason =
  | "owner_active"
  | "owner_identity_missing"
  | "discovery_open"
  | "discovery_incomplete"
  | "process_identity_missing"
  | "process_live"
  | "inspection_unknown"
  | "ownership_unreadable"
  | "generation_changed"
  | "recovery_busy"
  | "verified_stopped";

export interface WorkspaceRecoveryRecord {
  marker: "exclusive" | "quarantine";
  executionAttemptId: string;
  status: "recovered" | "blocked";
  reason: WorkspaceRecoveryReason;
}

export interface WorkspaceRecoveryReport {
  state: "clear" | "recovered" | "blocked";
  cleanup: ExecutionCleanup[];
  records: WorkspaceRecoveryRecord[];
}
