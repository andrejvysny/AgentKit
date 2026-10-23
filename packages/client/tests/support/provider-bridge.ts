import type {
  ToolSetContributor,
  ToolContributionContext,
} from "@agentkit/host";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";

/** Only test fixtures expose callbacks; production transports never import this. */
export function startProviderBridge(
  provider: AiProviderClient,
  contributors: ToolSetContributor[] = [],
) {
  const tools = new Map<string, AiTool>();
  const token = crypto.randomUUID();
  const controllers = new Set<AbortController>();
  const pumps = new Set<Promise<void>>();
  let readEvents:
    | ((runId: string, options: unknown) => Promise<unknown>)
    | undefined;
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    idleTimeout: 0,
    async fetch(request) {
      if (request.headers.get("authorization") !== `Bearer ${token}`)
        return new Response(null, { status: 401 });
      if (new URL(request.url).pathname === "/events-read") {
        const input = (await request.json()) as {
          runId: string;
          options: unknown;
        };
        if (readEvents === undefined)
          return new Response(null, { status: 503 });
        try {
          return Response.json(await readEvents(input.runId, input.options), {
            headers: { connection: "close" },
          });
        } catch {
          return new Response(null, { status: 500 });
        }
      }
      const path = new URL(request.url).pathname;
      if (path === "/contribute") {
        const input = (await request.json()) as {
          index: number;
          context: ToolContributionContext;
        };
        const contributor = contributors[input.index];
        if (contributor === undefined)
          return new Response(null, { status: 404 });
        const contributed = await contributor.contribute({
          ...input.context,
          signal: request.signal,
        });
        for (const tool of contributed)
          tools.set(
            `${input.context.runId}:${input.index}:${tool.definition.name}`,
            tool,
          );
        return Response.json(
          contributed.map((tool) => tool.definition),
          { headers: { connection: "close" } },
        );
      }
      if (path === "/tool") {
        const input = (await request.json()) as {
          index: number;
          name: string;
          context: AiToolExecutionContext;
          input: unknown;
        };
        const tool = tools.get(
          `${input.context.runId}:${input.index}:${input.name}`,
        );
        if (tool === undefined) return new Response(null, { status: 404 });
        return Response.json(
          await tool.execute(
            { ...input.context, signal: request.signal },
            input.input,
          ),
          { headers: { connection: "close" } },
        );
      }
      const input = (await request.json()) as AiChatRequest;
      const abort = new AbortController();
      controllers.add(abort);
      request.signal.addEventListener("abort", () => abort.abort(), {
        once: true,
      });
      const encoder = new TextEncoder();
      let closed = false;
      const stream = new ReadableStream<Uint8Array>({
        start(controller) {
          const pump = async () => {
            try {
              for await (const event of provider.streamChat({
                ...input,
                signal: abort.signal,
              })) {
                if (abort.signal.aborted) break;
                controller.enqueue(
                  encoder.encode(`${JSON.stringify(event)}\n`),
                );
              }
            } catch (error) {
              if (!abort.signal.aborted) {
                const message =
                  error instanceof Error ? error.message : String(error);
                const code =
                  error instanceof Error &&
                  "code" in error &&
                  typeof error.code === "string"
                    ? error.code
                    : "provider_error";
                controller.enqueue(
                  encoder.encode(
                    `${JSON.stringify({ bridgeError: { code, message } })}\n`,
                  ),
                );
              }
            } finally {
              controllers.delete(abort);
              if (!closed) {
                closed = true;
                controller.close();
              }
            }
          };
          const running = pump();
          pumps.add(running);
          void running.finally(() => pumps.delete(running));
        },
        cancel() {
          closed = true;
          abort.abort();
        },
      });
      return new Response(stream, {
        headers: {
          "content-type": "application/x-ndjson",
          connection: "close",
        },
      });
    },
  });
  return {
    url: `http://127.0.0.1:${server.port}/provider`,
    setEventsReader(
      reader: (runId: string, options: unknown) => Promise<unknown>,
    ) {
      readEvents = reader;
    },
    token,
    get activeCallbacks() {
      return pumps.size;
    },
    async stop() {
      for (const controller of controllers) controller.abort();
      await Promise.allSettled(pumps);
      // Bun can retain the stop promise after an aborted streaming response.
      // Callback completion is the cleanup boundary; force-close and unref the listener.
      void server.stop(true);
      server.unref();
    },
  };
}
