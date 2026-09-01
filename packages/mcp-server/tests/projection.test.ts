import { describe, expect, it } from "bun:test";
import type { AiToolEnvelope } from "@agentkit/contracts";
import type { ToolCatalogEntry } from "@agentkit/host";
import {
  isWriteTool,
  projectEnvelope,
  projectToolDefinition,
  toolEnvelopeFromResult,
  toolErrorEnvelope,
  visibleEntries,
} from "../src/index.js";
import { echoTool, textBlock, writeTool } from "./helpers.js";

function entry(tool: { definition: ToolCatalogEntry["definition"] }) {
  return { namespace: "demo", definition: tool.definition };
}

describe("projectToolDefinition", () => {
  it("carries name, description and inputSchema verbatim", () => {
    const definition = echoTool().definition;
    const projected = projectToolDefinition(entry(echoTool()));
    expect(projected.name).toBe(definition.name);
    expect(projected.description).toBe(definition.description);
    // Verbatim: the same JSON Schema object, not a regenerated one.
    expect(projected.inputSchema).toEqual(
      definition.inputSchema as unknown as typeof projected.inputSchema,
    );
    expect(Object.keys(projected).sort()).toEqual([
      "description",
      "inputSchema",
      "name",
    ]);
  });

  it("does not prefix the name with the contributor namespace", () => {
    const projected = projectToolDefinition({
      namespace: "demo",
      definition: echoTool().definition,
    });
    expect(projected.name).toBe("demo_echo");
    expect(projected.name).not.toContain("demo__");
  });

  it("does not forward outputSchema, which it would not honour", () => {
    const definition = {
      ...echoTool().definition,
      outputSchema: { type: "object" as const, properties: {} },
    };
    const projected = projectToolDefinition({ namespace: "demo", definition });
    expect("outputSchema" in projected).toBe(false);
  });
});

describe("write filtering", () => {
  const entries = [entry(echoTool()), entry(writeTool())];

  it("reads the write marker off AiToolDefinition.effect", () => {
    expect(isWriteTool(entry(writeTool()))).toBe(true);
    expect(isWriteTool(entry(echoTool()))).toBe(false);
  });

  it("drops write tools when writes are disabled", () => {
    expect(
      visibleEntries(entries, false).map((e) => e.definition.name),
    ).toEqual(["demo_echo"]);
  });

  it("keeps them when writes are enabled", () => {
    expect(visibleEntries(entries, true).map((e) => e.definition.name)).toEqual(
      ["demo_echo", "demo_write"],
    );
  });
});

describe("projectEnvelope", () => {
  const ok: AiToolEnvelope = {
    ok: true,
    status: "ok",
    summary: "echoed 2 char(s)",
    warnings: [],
    truncated: false,
    data: { echo: "hi" },
  };

  it("puts the summary first, then the payload as JSON", () => {
    expect(projectEnvelope(ok)).toEqual({
      content: [
        { type: "text", text: "echoed 2 char(s)" },
        { type: "text", text: '{"echo":"hi"}' },
      ],
    });
  });

  it("omits the summary block when there is none", () => {
    const { summary: _summary, ...noSummary } = ok;
    expect(projectEnvelope(noSummary).content).toEqual([
      { type: "text", text: '{"echo":"hi"}' },
    ]);
  });

  it("falls back to the whole envelope when data is absent", () => {
    const noData = { ...ok, data: undefined };
    const result = projectEnvelope(noData);
    expect(JSON.parse(textBlock(result.content, 1))).toMatchObject({
      ok: true,
      status: "ok",
    });
  });

  it("marks a failure isError and carries the structured error data", () => {
    const failed = toolErrorEnvelope("schema_invalid", "bad args", {
      phase: "validation",
      retryable: true,
    });
    const result = projectEnvelope(failed);
    expect(result.isError).toBe(true);
    expect(JSON.parse(textBlock(result.content, 1))).toEqual({
      errorCode: "schema_invalid",
      errorMessage: "bad args",
      phase: "validation",
      retryable: true,
    });
  });

  it("marks a partial isError too — MCP has no third state", () => {
    const partial: AiToolEnvelope = {
      ok: false,
      status: "partial",
      summary: "half applied",
      warnings: [],
      truncated: false,
      data: { appliedCount: 1 },
    };
    expect(projectEnvelope(partial).isError).toBe(true);
  });

  it("reports an unserializable payload instead of throwing", () => {
    const cyclic: Record<string, unknown> = {};
    cyclic["self"] = cyclic;
    const result = projectEnvelope({ ...ok, data: cyclic });
    expect(JSON.parse(textBlock(result.content, 1))).toMatchObject({
      errorCode: "tool_result_unserializable",
    });
  });
});

describe("toolEnvelopeFromResult", () => {
  const base = {
    sources: [],
    warnings: [],
    truncated: false,
    limits: { profile: "small" as const, maxBytes: 1000 },
  };

  it("prefers modelData over data", () => {
    const envelope = toolEnvelopeFromResult({
      ...base,
      ok: true,
      data: { verbose: true },
      modelData: { slim: true },
    });
    expect(envelope.data).toEqual({ slim: true });
  });

  it("keeps status partial even when ok is false", () => {
    expect(
      toolEnvelopeFromResult({
        ...base,
        ok: false,
        status: "partial",
        data: {},
      }).status,
    ).toBe("partial");
  });

  it("maps a plain ok:false onto error", () => {
    expect(
      toolEnvelopeFromResult({ ...base, ok: false, data: {} }).status,
    ).toBe("error");
  });
});
