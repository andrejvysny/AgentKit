/**
 * The managed-integrations recipe — an intentionally RETAINED TypeScript
 * slice of this example's backend, not a missed migration.
 *
 * Why it stays TS (capability gaps in `@agentkit/native`, each with its
 * evidence):
 *  - MCP OAuth: the binding's MCP client has no OAuth — the milestone doc
 *    (`docs/rust/native-binding-milestone.md`, "Open gaps") and
 *    `NativeMcpClientConfig.secretRefs` in `packages/native/src/mcp.ts`
 *    (deny-all: any `secretRefs` entry fails the connect with
 *    `mcp_secret_missing`). `ConnectionService` here runs the OAuth
 *    transport factory and revoke-on-disconnect against a
 *    `ManagedOAuthProvider`; there is nothing native to hand that to.
 *  - Managed content services (connections, prompt templates, skills): the
 *    Rust desktop backend answers those frozen routes with
 *    "Native managed content services are not configured."
 *    (`rust/crates/host/src/http/management.rs`), so this layer's own
 *    services — `ConnectionService`, `PromptTemplateService`,
 *    `SkillService`, `createRunContentProvider` — are the implementation,
 *    on stores the embedding app supplies.
 *  - Per-call connector approval (`approveExactAction`) and operation
 *    receipts have no native entry point either: the binding's documented
 *    reduction says remote MCP calls carry no run-scoped guard/receipt
 *    layers, and the engine has no standalone tool-dispatch entry point.
 *
 * What the native composition DOES take: the stdio MCP bridge and the
 * hosted MCP server (see `./wiring.ts`). This module is the opt-in recipe
 * for applications with their own settings and approval UI; nothing in the
 * REST/MCP hot path depends on it.
 */
import type {
  SecretStore,
  PromptTemplateStore,
  SkillSource,
  ToolContributionContext,
  RunContentContext,
} from "@agentkit/host";
import {
  PromptTemplateService,
  SkillService,
  createRunContentProvider,
} from "@agentkit/host";
import {
  ConnectionService,
  referenceConnectors,
  oauthTransportFactory,
  type ConnectionStore,
  type ConnectionGrant,
  type ConnectorInvocation,
  type OperationReceipt,
  type ManagedOAuthProvider,
  type McpModelContentCapabilities,
} from "@agentkit/mcp-client";

/** Optional recipe for applications with their own settings and approval UI. */
export function createManagedIntegrations(options: {
  secrets: SecretStore;
  connectionStore: ConnectionStore;
  templateStore: PromptTemplateStore;
  skillSources: readonly SkillSource[];
  selectedConnections(
    ctx: ToolContributionContext,
  ): Promise<readonly ConnectionGrant[]>;
  authorizedConnectors(ctx: RunContentContext): Promise<readonly string[]>;
  approveExactAction(invocation: ConnectorInvocation): Promise<boolean>;
  recordOperation(receipt: OperationReceipt): Promise<void>;
  modelContentCapabilities?(
    ctx: ToolContributionContext,
  ): McpModelContentCapabilities | Promise<McpModelContentCapabilities>;
  oauthProviderForAlias?(alias: string): ManagedOAuthProvider | undefined;
}) {
  const connections = new ConnectionService({
    secrets: options.secrets,
    store: options.connectionStore,
    connectors: referenceConnectors,
    authorize: options.approveExactAction,
    recordOperation: options.recordOperation,
    modelContentCapabilities: options.modelContentCapabilities,
    ...(options.oauthProviderForAlias
      ? {
          transportFactory: oauthTransportFactory(
            options.oauthProviderForAlias,
          ),
          revoke: async (connection) => {
            await options
              .oauthProviderForAlias?.(connection.config.alias)
              ?.disconnect();
          },
        }
      : {}),
  });
  const templates = new PromptTemplateService(options.templateStore);
  const skills = new SkillService(options.skillSources);
  return {
    connections,
    templates,
    skills,
    contributor: connections.contributor(options.selectedConnections),
    runContent: createRunContentProvider({
      templates,
      skills,
      authorizedConnectors: options.authorizedConnectors,
    }),
    rest: { connections, promptTemplates: templates, skills },
    dispose: () => connections.dispose(),
  };
}

export { createManagedConnectorAcceptance } from "./managed-connector-acceptance.js";
