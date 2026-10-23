import {
  LATEST_PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
} from "@modelcontextprotocol/sdk/types.js";
import { McpError } from "./errors.js";
export const MCP_COMPATIBILITY = Object.freeze({
  sdk: "@modelcontextprotocol/sdk@1.30.x",
  latest: LATEST_PROTOCOL_VERSION,
  supported: Object.freeze([...SUPPORTED_PROTOCOL_VERSIONS]),
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
