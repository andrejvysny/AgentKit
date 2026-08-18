export type AiSourceRefKind =
  | "design"
  | "schematic"
  | "pcb"
  | "net"
  | "part"
  | "library-component"
  | "symbol"
  | "footprint"
  | "file"
  | "tool"
  | "external";

export interface AiSourceRef {
  id: string;
  kind: AiSourceRefKind;
  label: string;
  refId?: string;
  path?: string;
  excerpt?: string;
  metadata?: Record<string, unknown>;
}
