import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import type { AiToolDefinition } from "@agentkit/contracts";
import {
  ResponsesClient,
  type AiTool,
  type ResponsesTransportRequest,
  type TrustedResponsesTransport,
} from "@agentkit/core";
import { fixture, eventStream } from "./provider-continuation-helpers.js";
import type { UsageRecord } from "../src/index.js";

export interface OpenPcbCatalog {
  provenance: {
    commit: string;
    sourceRawSha256: string;
    sourceCanonicalDataSha256: string;
    toolsSha256: string;
  };
  tools: AiToolDefinition[];
}

export const catalog: OpenPcbCatalog = JSON.parse(
  readFileSync(
    new URL("../../testing/fixtures/openpcb/catalog.json", import.meta.url),
    "utf8",
  ),
);
export const toolName = "designer_get_design_summary";
export const callId = "openpcb-summary:call/17";
export const argumentsJson = '{"designId":"fixture-design"}';
export const nativeCall = {
  type: "function_call",
  id: "native-provider-item",
  call_id: callId,
  namespace: "agentkit",
  name: toolName,
  arguments: argumentsJson,
  status: "completed",
};

export function catalogDigest(): string {
  return createHash("sha256")
    .update(JSON.stringify(catalog.tools))
    .digest("hex");
}

function nativeTools(
  definitions: AiToolDefinition[],
  executed: unknown[],
): AiTool[] {
  return definitions.map((definition) => ({
    definition,
    async execute(context, input) {
      if (definition.name !== toolName)
        throw new Error(
          `Unexpected native fixture execution: ${definition.name}`,
        );
      executed.push(input);
      return {
        ok: true,
        data: { designId: "fixture-design", localDetail: "host-only" },
        modelData: { designId: "fixture-design", parts: 17 },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  }));
}

export async function consumerFixture(
  authKind: TrustedResponsesTransport["authKind"],
  output: (call: number) => unknown[],
  definitions: AiToolDefinition[] = catalog.tools,
) {
  const sent: ResponsesTransportRequest[] = [];
  const executed: unknown[] = [];
  const recordedUsage: UsageRecord[] = [];
  const environment = await fixture({
    overrides: {
      contributors: [
        {
          namespace: "openpcb",
          contribute: async () => nativeTools(definitions, executed),
        },
      ],
      usage: {
        authorize: async () => ({ allowed: true }),
        async record(usage) {
          recordedUsage.push(usage);
        },
      },
      providerFactory: () =>
        new ResponsesClient({
          id: "provider",
          transport: {
            identity: {
              providerId: "provider",
              protocol: "responses",
              connectionId: "openpcb-fixture",
              generation: 1,
            },
            authKind,
            async request(input) {
              sent.push(input);
              return eventStream(output(sent.length));
            },
          },
        }),
    },
  });
  return { ...environment, sent, nativeExecuted: executed, recordedUsage };
}
