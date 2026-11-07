// Hand-rolled MCP stdio peer for the client-side request-cancellation gate
// (AGENTKIT-80): it records EVERY JSON-RPC line it receives, verbatim, one
// per line into `CAPTURE_FILE`, answers `initialize`, and DELIBERATELY NEVER
// ANSWERS `tools/call` — the pending response stands in for a server whose
// tool body outlives the client's deadline or patience. The test then reads
// the capture back to prove the client wrote a spec-shaped
// `notifications/cancelled` for that request id.
//
// Deliberately not the SDK: the capture must be byte-level evidence of what
// travelled on the request channel, not what a peer library chose to surface.
import { appendFileSync, writeFileSync } from "node:fs";

const capture = process.env.CAPTURE_FILE;
if (!capture) {
  console.error("cancel-capture-stdio.mjs requires CAPTURE_FILE");
  process.exit(1);
}
writeFileSync(capture, "");

const KNOWN = new Set(["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]);

function respond(id, result) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  for (;;) {
    const newline = buffer.indexOf("\n");
    if (newline < 0) break;
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    if (line.length === 0) continue;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      continue;
    }
    appendFileSync(capture, `${line}\n`);
    if (message.method === "initialize") {
      respond(message.id, {
        protocolVersion: KNOWN.has(message.params?.protocolVersion)
          ? message.params.protocolVersion
          : "2025-11-25",
        capabilities: { tools: {} },
        serverInfo: { name: "cancel-capture-fixture", version: "1" },
      });
    }
    // `notifications/initialized`, `tools/call`, and everything else are
    // recorded only; no `tools/call` response is ever written. With
    // EXIT_ON_CALL the peer dies right after recording the call, standing in
    // for a dead response channel.
    if (message.method === "tools/call" && process.env.EXIT_ON_CALL === "1") {
      process.exit(0);
    }
  }
});

process.stdin.on("end", () => process.exit(0));
// Keep the child alive with a pending call outstanding; the client kills it
// when the transport drops (kill_on_drop), exactly like a real hung server.
setInterval(() => {}, 1000);
