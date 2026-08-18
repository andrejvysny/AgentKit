export type AiContextBindingKind =
  | "design"
  | "library-component"
  | "symbol"
  | "footprint"
  | "file"
  | "selection"
  | "net"
  | "part";

export type AiContextBindingRole = "primary" | "reference" | "comparison";

export type AiContextBindingStatus = "active" | "missing" | "stale";

export interface AiContextBinding {
  id: string;
  kind: AiContextBindingKind;
  refId: string;
  label: string;
  role: AiContextBindingRole;
  status: AiContextBindingStatus;
  metadata?: Record<string, unknown>;
}
