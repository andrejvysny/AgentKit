import * as contracts from "agentkit/contracts";
import * as core from "agentkit/core";
import * as host from "agentkit/host";
import * as sqlite from "agentkit/adapters-sqlite-node";
import * as runner from "agentkit/runner-local";
import * as transport from "agentkit/transport-http";
import * as client from "agentkit/client";
import * as react from "agentkit/react";
import * as mcpClient from "agentkit/mcp-client";
import * as mcpServer from "agentkit/mcp-server";

const entries: readonly object[] = [
  contracts,
  core,
  host,
  sqlite,
  runner,
  transport,
  client,
  react,
  mcpClient,
  mcpServer,
];
void entries;
