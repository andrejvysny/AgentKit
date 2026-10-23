import { SUPPORTED_PROTOCOL_VERSIONS } from "@modelcontextprotocol/client";
import { McpError } from "./errors.js";
export const MCP_COMPATIBILITY = Object.freeze({
  sdk: "@modelcontextprotocol/client,server,core@2.4.0",
  latest: "2026-07-28",
  supported: Object.freeze(["2026-07-28", ...SUPPORTED_PROTOCOL_VERSIONS]),
  negotiation:
    "official SDK automatic discovery with legacy initialize fallback",
  verified: Object.freeze({
    inboundHttp: Object.freeze(["2026-07-28", ...SUPPORTED_PROTOCOL_VERSIONS]),
    outboundHttp: Object.freeze(["2026-07-28", "2025-11-25"]),
    outboundStdio: Object.freeze(["2026-07-28", "2025-11-25"]),
    runtimes: Object.freeze(["Node 22", "Bun 1.4"]),
    elicitation: "modern input_required; callback capability and cancellation",
  }),
  actorIdentity:
    "application-generated; independent of MCP session identifiers",
});
export function assertSupportedMcpProtocol(version: string): void {
  if (!MCP_COMPATIBILITY.supported.includes(version))
    throw new McpError(
      "mcp_unsupported_protocol",
      `Unsupported MCP protocol revision: ${version}`,
    );
}
