import { describe, expect, it } from "bun:test";
import {
  buildMcpToolIdentity,
  buildMcpToolIdentityIndex,
  McpError,
  parseMcpCanonicalToolId,
  toRegistryToolName,
} from "../src/index.js";

function codeOf(fn: () => unknown): string {
  try {
    fn();
  } catch (err) {
    return err instanceof McpError ? err.code : `not-mcp:${String(err)}`;
  }
  return "no-throw";
}

describe("canonical tool identity", () => {
  it("builds mcp.<alias>.<tool> and a registry-legal projection", () => {
    const identity = buildMcpToolIdentity({
      serverAlias: "github",
      toolName: "list_issues",
    });
    expect(identity.canonicalId).toBe("mcp.github.list_issues");
    expect(identity.registryName).toBe("mcp__github__list_issues");
    expect(identity.effectiveToolName).toBe("list_issues");
  });

  it("applies the tool alias to the canonical id but keeps the server-side name", () => {
    const identity = buildMcpToolIdentity({
      serverAlias: "github",
      toolName: "list_issues",
      toolAlias: "issues",
    });
    expect(identity.canonicalId).toBe("mcp.github.issues");
    expect(identity.toolName).toBe("list_issues");
    expect(identity.effectiveToolName).toBe("issues");
  });

  it("keeps dotted tool names, projecting every dot for the registry", () => {
    const identity = buildMcpToolIdentity({
      serverAlias: "fs",
      toolName: "files.read",
    });
    expect(identity.canonicalId).toBe("mcp.fs.files.read");
    expect(identity.registryName).toBe("mcp__fs__files__read");
    expect(toRegistryToolName(identity.canonicalId)).toBe(identity.registryName);
  });

  it("rejects aliases and tool names outside the grammar", () => {
    expect(codeOf(() => buildMcpToolIdentity({ serverAlias: "GitHub", toolName: "x" })))
      .toBe("mcp_invalid_alias");
    expect(codeOf(() => buildMcpToolIdentity({ serverAlias: "9gh", toolName: "x" })))
      .toBe("mcp_invalid_alias");
    expect(codeOf(() => buildMcpToolIdentity({ serverAlias: "gh", toolName: "List" })))
      .toBe("mcp_invalid_tool_name");
    expect(codeOf(() => buildMcpToolIdentity({ serverAlias: "gh", toolName: "a b" })))
      .toBe("mcp_invalid_tool_name");
    expect(
      codeOf(() =>
        buildMcpToolIdentity({ serverAlias: "gh", toolName: "ok", toolAlias: "No!" }),
      ),
    ).toBe("mcp_invalid_tool_name");
  });

  it("round-trips through parse, including dotted tool names", () => {
    expect(parseMcpCanonicalToolId("mcp.fs.files.read")).toEqual({
      serverAlias: "fs",
      effectiveToolName: "files.read",
    });
  });

  it("refuses ids that are not mcp.<alias>.<tool>", () => {
    expect(codeOf(() => parseMcpCanonicalToolId("search"))).toBe(
      "mcp_invalid_canonical_id",
    );
    expect(codeOf(() => parseMcpCanonicalToolId("mcp.gh"))).toBe(
      "mcp_invalid_canonical_id",
    );
    expect(codeOf(() => parseMcpCanonicalToolId("mcp.gh."))).toBe(
      "mcp_invalid_canonical_id",
    );
    expect(codeOf(() => parseMcpCanonicalToolId("other.gh.search"))).toBe(
      "mcp_invalid_canonical_id",
    );
  });

  it("fails the whole batch when two tools collapse onto one canonical id", () => {
    const build = (): unknown =>
      buildMcpToolIdentityIndex([
        { serverAlias: "gh", toolName: "list_issues", toolAlias: "issues" },
        { serverAlias: "gh", toolName: "search_issues", toolAlias: "issues" },
      ]);
    expect(codeOf(build)).toBe("mcp_canonical_id_collision");
    try {
      build();
    } catch (err) {
      // Both sides are named: an operator has to know WHICH aliases to fix.
      expect((err as McpError).message).toContain("gh:list_issues");
      expect((err as McpError).message).toContain("gh:search_issues");
    }
  });

  it("fails the batch when two canonical ids project onto one registry name", () => {
    // `.` -> `__` is many-to-one, and the registry name is what the model
    // calls. Deduping on the canonical id alone lets this pair through, and one
    // tool then shadows the other during registry staging.
    const build = (): unknown =>
      buildMcpToolIdentityIndex([
        { serverAlias: "fs", toolName: "files.read" },
        { serverAlias: "fs", toolName: "files__read" },
      ]);
    expect(codeOf(build)).toBe("mcp_canonical_id_collision");
    try {
      build();
    } catch (err) {
      const message = (err as McpError).message;
      // Both canonical ids, or the message reads as a contradiction: they are
      // NOT the same id, and the operator has to see which two to reconcile.
      expect(message).toContain("mcp.fs.files.read");
      expect(message).toContain("mcp.fs.files__read");
      expect(message).toContain("mcp__fs__files__read");
    }
  });

  it("accepts a batch whose aliases stay distinct", () => {
    const index = buildMcpToolIdentityIndex([
      { serverAlias: "gh", toolName: "list_issues", toolAlias: "issues" },
      { serverAlias: "gh", toolName: "search_issues" },
    ]);
    expect([...index.keys()]).toEqual([
      "mcp.gh.issues",
      "mcp.gh.search_issues",
    ]);
  });
});
