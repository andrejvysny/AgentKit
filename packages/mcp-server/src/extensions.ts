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

/** Host catalogues authorize visibility; callers cannot guess hidden names. */
export function installHostExtensions(
  server: Server,
  options: McpServerHandlerOptions,
  scope: McpSessionScope,
  lifecycle: { begin(): void; finish(): void },
): void {
  const resources = options.resources;
  if (resources !== undefined) {
    server.setRequestHandler(ListResourcesRequestSchema, () =>
      invoke("resources/list", () =>
        resources.list(scope).then((items) => ({ resources: items })),
      ),
    );
    server.setRequestHandler(ReadResourceRequestSchema, (request) =>
      invoke("resources/read", async () => {
        const listed = await resources.list(scope);
        if (!listed.some((item) => item.uri === request.params.uri)) {
          throw new McpError(ErrorCode.InvalidParams, "Unknown resource");
        }
        return resources.read(request.params.uri, scope);
      }),
    );
  }
  const prompts = options.prompts;
  if (prompts !== undefined) {
    server.setRequestHandler(ListPromptsRequestSchema, () =>
      invoke("prompts/list", () =>
        prompts.list(scope).then((items) => ({ prompts: items })),
      ),
    );
    server.setRequestHandler(GetPromptRequestSchema, (request) =>
      invoke("prompts/get", async () => {
        const listed = await prompts.list(scope);
        const prompt = listed.find((item) => item.name === request.params.name);
        if (prompt === undefined)
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
        ) {
          throw new McpError(
            ErrorCode.InvalidParams,
            "Invalid prompt arguments",
          );
        }
        return prompts.get(request.params.name, args, scope);
      }),
    );
  }

  async function invoke<T>(
    method: string,
    operation: () => Promise<T>,
  ): Promise<T> {
    lifecycle.begin();
    try {
      scope.signal?.throwIfAborted();
      return await operation();
    } catch (error) {
      if (error instanceof McpError) throw error;
      options.logger?.error("mcp host extension failed", {
        method,
        actorId: scope.actorId,
        error: String(error),
      });
      throw new McpError(ErrorCode.InternalError, "Host MCP extension failed");
    } finally {
      lifecycle.finish();
    }
  }
}
