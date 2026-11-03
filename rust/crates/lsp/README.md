# AgentKit native LSP tools

A native Language Server Protocol client, language-server lifecycle manager
and the AgentKit `agentkit_core::Tool` contribution — no external LSP crate;
JSON-RPC 2.0 framing is implemented here.

## Pieces

- `jsonrpc` — `Content-Length` framed JSON-RPC 2.0 over stdio with request
  routing by id, server-initiated requests answered on a spawned task
  (never blocking the reader), and a broken-connection model that fails
  every in-flight request.
- `server` — per-generation lifecycle: `initialize`/`initialized`
  handshake, crash detection, bounded restart backoff
  (`lsp_server_unavailable` until the cooldown elapses), per-request
  timeout with `$/cancelRequest`, caller-token cancellation and graceful
  `shutdown`/`exit` with SIGKILL fallback.
- `documents` — lazily opened documents with content-hash change detection
  (full-sync `didChange`), push `publishDiagnostics` registry and
  workspace-confinement of tool file inputs.
- `discovery` — `rust` (rust-analyzer) and `typescript`
  (typescript-language-server, ancestor `node_modules/.bin` walk) candidate
  resolution with `AGENTKIT_LSP_RUST` / `AGENTKIT_LSP_TYPESCRIPT` overrides
  and explicit `servers` configuration. Nothing is ever installed.
- `pool` — `LspPool` shares one server process per (canonical workspace
  root, language), with idle eviction and bounded `close`.
- `tools` — `lsp_definition`, `lsp_references`, `lsp_hover`, `lsp_symbols`,
  `lsp_implementation`, `lsp_diagnostics` (all effect `read`; positions are
  1-based in the tool interface, LSP positions are 0-based).

## Diagnostic codes

`lsp_server_not_found`, `lsp_server_crashed`, `lsp_server_unavailable`,
`lsp_request_timeout`, `lsp_invalid_file`, `lsp_stale_result`,
`lsp_protocol_error`, `lsp_invalid_input`, `lsp_server_error`, `cancelled`.

## Host usage

```rust
let pool = LspPool::new(LspConfig::from_json(binding_json)?);
for tool in lsp_tools(Arc::clone(&pool), workspace_root) {
    registry.register(tool)?;
}
// shutdown when the session ends:
pool.close().await;
```
