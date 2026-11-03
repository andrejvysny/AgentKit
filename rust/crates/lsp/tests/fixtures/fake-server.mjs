// Minimal LSP server for lifecycle tests, driven by environment flags:
//   LSP_FAKE_EXIT_EARLY=1     exit immediately without speaking (init EOF)
//   LSP_FAKE_HANG=<method>    accept then never answer that method
//   LSP_FAKE_CRASH_ON=<method> exit(1) mid-request (marker-gated once)
//   LSP_FAKE_MARKER=<file>     marker path so the respawned server behaves
//   LSP_FAKE_SILENT=<method>   same as hang (alias)
// Everything else is answered from canned, deterministic payloads.
import fs from "node:fs";

let buffer = Buffer.alloc(0);
const pendingChunks = [];
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  while (true) {
    const headerEnd = buffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const headers = buffer.subarray(0, headerEnd).toString("latin1");
    const match = /content-length:\s*(\d+)/i.exec(headers);
    if (!match) {
      process.stderr.write("missing content-length\n");
      process.exit(2);
    }
    const length = parseInt(match[1], 10);
    if (buffer.length < headerEnd + 4 + length) return;
    const body = buffer
      .subarray(headerEnd + 4, headerEnd + 4 + length)
      .toString("utf8");
    buffer = buffer.subarray(headerEnd + 4 + length);
    onMessage(JSON.parse(body));
  }
});

function send(message) {
  const body = JSON.stringify(message);
  const frame = `Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`;
  process.stdout.write(frame);
}

function reply(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

const hangOn = process.env.LSP_FAKE_HANG || process.env.LSP_FAKE_SILENT;
const crashOn = process.env.LSP_FAKE_CRASH_ON;
const marker = process.env.LSP_FAKE_MARKER;
const publishDiagnostics = process.env.LSP_FAKE_PUBLISH === "1";
const logPath = process.env.LSP_FAKE_LOG;

function onMessage(message) {
  const { id, method, params } = message;
  if (logPath)
    fs.appendFileSync(logPath, JSON.stringify({ id, method, params }) + "\n");
  if (id === undefined) {
    if (method === "exit") process.exit(0);
    if (method === "textDocument/didOpen" && publishDiagnostics) {
      send({
        method: "textDocument/publishDiagnostics",
        params: {
          uri: params.textDocument.uri,
          diagnostics: [
            {
              range: {
                start: { line: 1, character: 0 },
                end: { line: 1, character: 5 },
              },
              severity: 1,
              code: "FAKE001",
              source: "fake",
              message: "deliberate fake error",
            },
          ],
        },
      });
    }
    return;
  }
  switch (method) {
    case "initialize":
      if (process.env.LSP_FAKE_EXIT_EARLY === "1") process.exit(1);
      if (hangOn === method) return;
      reply(id, {
        capabilities: {
          ...(process.env.LSP_FAKE_PULL === "1"
            ? {
                diagnosticProvider: { workDoneProgress: false, triggerKind: 1 },
              }
            : {}),
          definitionProvider: true,
          referencesProvider: true,
          hoverProvider: true,
          implementationProvider: true,
          workspaceSymbolProvider: true,
          textDocumentSync: 1,
        },
      });
      if (process.env.LSP_FAKE_EXIT_AFTER_INIT === "1") process.exit(1);
      return;
    case "shutdown":
      return reply(id, null);
    case "textDocument/definition":
      if (shouldCrash(method)) return;
      if (hangOn === method) return;
      return reply(id, [
        {
          uri: params.textDocument.uri,
          range: {
            start: { line: 12, character: 7 },
            end: { line: 12, character: 13 },
          },
        },
      ]);
    case "textDocument/references":
      return reply(id, [
        {
          uri: params.textDocument.uri,
          range: {
            start: { line: 17, character: 4 },
            end: { line: 17, character: 10 },
          },
        },
      ]);
    case "textDocument/hover":
      if (hangOn === method) return;
      return reply(id, {
        contents: {
          kind: "markdown",
          value: "```rust\nfn canned() -> u32\n```",
        },
      });
    case "textDocument/implementation":
      return reply(id, [
        {
          uri: params.textDocument.uri,
          range: {
            start: { line: 7, character: 4 },
            end: { line: 7, character: 9 },
          },
        },
      ]);
    case "workspace/symbol":
      return reply(id, [
        {
          name: "cannedSymbol",
          kind: 12,
          location: {
            uri: params.__uri || "file:///ws/src/lib.rs",
            range: {
              start: { line: 2, character: 7 },
              end: { line: 2, character: 19 },
            },
          },
        },
      ]);
    case "textDocument/diagnostic":
      return reply(id, {
        kind: "full",
        items: [
          {
            range: {
              start: { line: 0, character: 0 },
              end: { line: 0, character: 2 },
            },
            severity: 1,
            message: "pull fake error",
          },
        ],
      });
    case "test/slow":
      setTimeout(
        () => reply(id, { slow: true }),
        Number(params?.delayMs ?? 250),
      );
      return;
    default:
      return reply(id, { id: method });
  }
}

function shouldCrash(method) {
  if (crashOn !== method) return false;
  if (marker && fs.existsSync(marker)) return false; // respawned: behave
  if (marker) fs.writeFileSync(marker, "crashed");
  process.exit(1);
}
