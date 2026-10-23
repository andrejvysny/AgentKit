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
  oauthProviderForAlias?(alias: string): ManagedOAuthProvider | undefined;
}) {
  const connections = new ConnectionService({
    secrets: options.secrets,
    store: options.connectionStore,
    connectors: referenceConnectors,
    authorize: options.approveExactAction,
    recordOperation: options.recordOperation,
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
