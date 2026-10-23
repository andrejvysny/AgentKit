import type { Server } from "@modelcontextprotocol/sdk/server/index.js";
import {
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
  ListPromptsRequestSchema,
  GetPromptRequestSchema,
  McpError,
  ErrorCode,
} from "@modelcontextprotocol/sdk/types.js";
import type { McpServerHandlerOptions, McpSessionScope } from "./types.js";

/** Every extension uses the same actor scope and request cancellation boundary. */
export function installHostExtensions(
  server: Server,
  options: McpServerHandlerOptions,
  session: McpSessionScope,
  lifecycle: { begin(): void; finish(): void },
): void {
  const resources = options.resources;
  if (resources) {
    server.setRequestHandler(ListResourcesRequestSchema, (_request, extra) =>
      invoke("resources/list", extra.signal, async (scope) => ({
        resources: await resources.list(scope),
      })),
    );
    server.setRequestHandler(ReadResourceRequestSchema, (request, extra) =>
      invoke("resources/read", extra.signal, async (scope) => {
        const listed = await resources.list(scope);
        scope.signal?.throwIfAborted();
        if (!listed.some((item) => item.uri === request.params.uri))
          throw new McpError(ErrorCode.InvalidParams, "Unknown resource");
        return resources.read(request.params.uri, scope);
      }),
    );
  }
  const prompts = options.prompts;
  if (prompts) {
    server.setRequestHandler(ListPromptsRequestSchema, (_request, extra) =>
      invoke("prompts/list", extra.signal, async (scope) => ({
        prompts: await prompts.list(scope),
      })),
    );
    server.setRequestHandler(GetPromptRequestSchema, (request, extra) =>
      invoke("prompts/get", extra.signal, async (scope) => {
        const prompt = (await prompts.list(scope)).find(
          (item) => item.name === request.params.name,
        );
        scope.signal?.throwIfAborted();
        if (!prompt)
          throw new McpError(ErrorCode.InvalidParams, "Unknown prompt");
        const args = request.params.arguments ?? {};
        const allowed = new Set(
          prompt.arguments?.map((argument) => argument.name),
        );
        if (
          Object.keys(args).some((key) => !allowed.has(key)) ||
          prompt.arguments?.some(
            (argument) =>
              argument.required && args[argument.name] === undefined,
          )
        )
          throw new McpError(
            ErrorCode.InvalidParams,
            "Invalid prompt arguments",
          );
        return prompts.get(request.params.name, args, scope);
      }),
    );
  }
  async function invoke<T>(
    method: string,
    signal: AbortSignal,
    operation: (scope: McpSessionScope) => Promise<T>,
  ): Promise<T> {
    lifecycle.begin();
    const scope = {
      ...session,
      signal: session.signal
        ? AbortSignal.any([session.signal, signal])
        : signal,
    };
    try {
      scope.signal.throwIfAborted();
      return await operation(scope);
    } catch (error) {
      if (error instanceof McpError) throw error;
      options.logger?.error("mcp host extension failed", {
        method,
        actorId: session.actorId,
      });
      throw new McpError(ErrorCode.InternalError, "Host MCP extension failed");
    } finally {
      lifecycle.finish();
    }
  }
}
