import { Type, type Static } from "@sinclair/typebox";
import { McpTransportDtoSchema } from "./rest.js";
import { AiJsonSchemaObjectSchema } from "./json-schema.js";
export const ManagedConnectionConfigSchema = Type.Object({
  alias: Type.String(),
  transport: McpTransportDtoSchema,
  secretRefs: Type.Optional(Type.Record(Type.String(), Type.String())),
  enabled: Type.Optional(Type.Boolean()),
  toolAliases: Type.Optional(Type.Record(Type.String(), Type.String())),
  resilience: Type.Optional(
    Type.Object({
      requestTimeoutMs: Type.Optional(Type.Number()),
      connectTimeoutMs: Type.Optional(Type.Number()),
      maxConnectAttempts: Type.Optional(Type.Number()),
      connectBackoffBaseMs: Type.Optional(Type.Number()),
      connectBackoffMaxMs: Type.Optional(Type.Number()),
      circuitOpenMs: Type.Optional(Type.Number()),
      reconnectMaxAttempts: Type.Optional(Type.Number()),
      reconnectBackoffFactor: Type.Optional(Type.Number()),
      retryTimeouts: Type.Optional(Type.Boolean()),
    }),
  ),
  discovery: Type.Optional(
    Type.Object({
      maxPages: Type.Optional(Type.Integer({ minimum: 1 })),
      maxItems: Type.Optional(Type.Integer({ minimum: 1 })),
      maxBytes: Type.Optional(Type.Integer({ minimum: 1 })),
    }),
  ),
});
export const ConnectionStatusSchema = Type.Union(
  [
    "disconnected",
    "connecting",
    "connected",
    "needs_authentication",
    "failed",
  ].map((value) => Type.Literal(value)),
);
export type ConnectionStatus =
  | "disconnected"
  | "connecting"
  | "connected"
  | "needs_authentication"
  | "failed";
export const ConnectionRecordSchema = Type.Object({
  id: Type.String(),
  connectorId: Type.String(),
  accountId: Type.String(),
  workspaceId: Type.Optional(Type.String()),
  revision: Type.Integer({ minimum: 1 }),
  config: ManagedConnectionConfigSchema,
  status: ConnectionStatusSchema,
  errorCode: Type.Optional(Type.String()),
});
export type ConnectionRecord = Omit<
  Static<typeof ConnectionRecordSchema>,
  "status"
> & { status: ConnectionStatus };
export type SaveConnectionRequest = Omit<
  ConnectionRecord,
  "status" | "errorCode"
> & { expectedRevision: number | null };
export const PromptTemplateSchema = Type.Object({
  id: Type.String(),
  revision: Type.String(),
  description: Type.String(),
  argumentsSchema: AiJsonSchemaObjectSchema,
  messages: Type.Array(
    Type.Object({
      role: Type.Union([
        Type.Literal("system"),
        Type.Literal("user"),
        Type.Literal("assistant"),
      ]),
      content: Type.String(),
    }),
    { minItems: 1, maxItems: 100 },
  ),
  origin: Type.Optional(
    Type.Object({
      kind: Type.Union([Type.Literal("application"), Type.Literal("mcp")]),
      source: Type.String(),
      trusted: Type.Boolean(),
    }),
  ),
});
