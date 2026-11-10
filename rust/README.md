# AgentKit Rust

This workspace is an independent native implementation alongside the TypeScript
packages. The wire authority is the Rust `agentkit-contracts` crate (ADR 0022): contract `0.7.0`, REST `v1`, frozen at
`36577829cfbfd78e5b0492d826f46ba6131835b6`. It uses a new SQLite database identity
and does not open or migrate the TypeScript schema-9 database.

**Full parity is an acceptance gate, not a claim made by the existence of these
crates.** The inventory, frozen fixtures, reproduced baseline defects, and
remaining qualification requirements are in [the parity specification](../docs/rust/parity.md).

## Dependency boundaries

| Crate | Responsibility |
| --- | --- |
| `agentkit-kernel` | Foundation primitives: SHA-256 hex, canonical JSON and the conversation timestamp |
| `agentkit-contracts` | Portable Serde contracts, offline schema validation, routes and idempotency |
| `agentkit-core` | Provider protocols, streaming, bounded tool execution and budgets |
| `agentkit-engine` | Generic durable task execution (runner, retry, subscription), the task model and the `TaskRepository` port the store implements |
| `agentkit-store` | SQLite actor, atomic state changes, leases, projections and recovery |
| `agentkit-host` | Durable execution, agent sessions with steering/human input/plan grants, native run handles and proposal application |
| `agentkit-agents` | Agent definitions, teammate admission, subagent delegation and the advisor/watchdog over the host engine |
| `agentkit-connections` | Application namespaces, immutable selection, consent, secret-store and network ports |
| `agentkit-http` | Axum REST/SSE boundary, plus the desktop backend serving it from a local host |
| `agentkit-client` | Native HTTP client, replay decoding and presentation state |
| `agentkit-mcp` | Official `rmcp` transports with the frozen TypeScript protocol revisions |
| `agentkit-rust` | Feature-selected integration facade; contracts only by default |
| `agentkit-napi` | Node-API binding behind `@agentkit/native` (handles, promises, pull-based durable-event subscriptions); see [the binding milestone](../../docs/rust/native-binding-milestone.md) |

The facade's `runtime`, `durable`, `client`, `http`, `mcp`, and `keychain` features
select integrations explicitly. Contracts also compile for
`wasm32-unknown-unknown`; execution is native. Workspace dependencies are locked
in `Cargo.lock`, with Rust `1.97.0` pinned by `rust-toolchain.toml`.

## Embedding

Create a dedicated `SqliteStore`, register task executors, and construct an
`AgentHost` inside an asynchronous runtime. Database operations run on a dedicated
thread. Open the database and shut down its final owner away from the UI thread.
The store rejects a foreign/newer database and a second owning process.

Call `recover()` before starting the worker. `HostOptions` defaults to manual
recovery: interrupted work requires an explicit `resume()`. Call
`start_worker()` only once the application has installed its trusted providers,
tools, authorization and verification hooks. On application shutdown, await the
worker's `shutdown()` before releasing the database owner.

`RunHandle::subscribe()` follows committed events with a bounded channel. Closing
a subscription does not cancel execution. Use `RunHandle::cancel()` explicitly.
Provider terminal events leave the logical run settling; `RunUpdate::Settled`
and the HTTP `agentkit.stream.settled` frame follow durable settlement and the
final event drain, including verification. A stream asked to resume from an
unknown `Last-Event-ID` announces its full replay — the
`x-agentkit-stream-resync` header and an opening `agentkit.stream.resync`
control frame — byte-for-byte as the TypeScript transport does.

`ChatExecutor` uses the core's awaited event-persistence hook. Failed persistence
prevents the next tool/provider operation. Completed tool effects still need
domain receipts: cancellation, timeouts and transport exceptions do not imply
that an external mutation was rolled back. `ProposalService` validates proposal
identity and current authorization before returning a receipt, and reconciles
ambiguous application outcomes without automatically reapplying them.

## Connections and consent

Create an application-scoped connection repository. Use one canonical repository
for native selection and REST provider projections. Save versioned connection
metadata through the host; credentials belong in an injected secret store or the
optional platform keychain adapter. There is no automatic plaintext fallback.

An execution plan fixes the connection generation, destination, account, model,
capability, disclosed categories and request. Exact-payload consent is the
default; reusable scope consent is an explicit separate action. Changes to those
bindings invalidate previous plans. Credential reads happen only after adapter,
capability and consent checks. The native provider bridge also fences streaming
results when consent or selection changes.

The HTTP backend is a single-actor desktop boundary. Applications requiring
multiple tenants must supply an appropriately isolated `Backend`; a shared
unfiltered store is not a tenant boundary. Provider/account authentication and
domain mutation permission remain application responsibilities.

## Reference application

```sh
cd rust
cargo run -p agentkit-egui-host --locked
```

The reference app uses nonblocking channels between egui and native work. Its
offline demonstration is labeled explicitly; it is not evidence of live provider
or platform keychain qualification. Consult the example's documentation for its
database path, consent controls and supported provider configuration.

## Managed content and schema qualification

The current shared contracts contain 101 schemas and 48 REST operations. The
native desktop backend resolves selected published prompt templates and managed
skills before admission, freezes revisions and connector authority, and transmits
typed reasoning through native and callback providers. See
[native admission](../docs/rust/native-admission.md) for profiles, transcript and
context ceilings, Responses binding/replay behavior, and API migration. Optional
provider/MCP lifecycle services still require explicit host composition.

Tool schemas retain resource `$id` and declared dialects, while ordinary JSON
payloads in `const`, `enum`, defaults, and examples retain their literal keys.
Unknown validation keywords, unknown formats, unsupported dialects, and ambiguous
resource identities fail compilation. Supported dialect selection is Draft-07,
2019-09, and 2020-12; this is not a claim of arbitrary dialect parity.

The pinned validator's evaluated-property tracking bypasses the custom
ECMAScript matcher. Native compilation therefore rejects 2019-09/2020-12
schema documents combining `unevaluatedProperties` with `patternProperties`
unless every pattern is an ASCII literal, optionally beginning with `^` and/or
ending with `$`. Literal bodies allow letters, digits, `_`, `:`, `/`, and `-`.
This conservative document-wide check also covers nested and referenced schema
branches. Draft-07 ECMAScript property matching, modern literal patterns, and
modern schemas without that combination retain their normal validation paths.
A refused schema reports
`Native unevaluatedProperties requires literal patternProperties patterns`;
it is not silently downgraded or accepted as parity.

## Verification

For untrusted JSON, use `agentkit_contracts::parse_javascript_json` and the
validated contract decoders. `JsonDeserialize::from_json` and the
`deserialize_preserving` Serde field adapter preserve arbitrary object keys in
typed models. The native client's `call_typed` uses this trait. Direct generic
Serde decoding into `serde_json::Value` or standard maps does not provide the
same guarantees with arbitrary-precision numbers enabled.

Numeric input follows JavaScript binary64 semantics, including overflow at tool
callbacks; JSON output converts nonfinite values to null. Unpaired UTF-16
surrogates are the documented, approved UTF-8 compatibility exception.

From the monorepo root:

```sh
bun run lint
bun scripts/export-rust-fixtures.ts --check
bun test
bun run typecheck
bun test --timeout 60000 packages/client/tests packages/react/tests # Rust fixture is the only backend
AGENTKIT_RUN_RUST_CONFORMANCE=1 AGENTKIT_RUN_RUST_RUNNER_CONFORMANCE=1 bun test scripts/tests/rust-store-conformance.test.ts scripts/tests/rust-runner-conformance.test.ts
cd rust
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo check -p agentkit-contracts --target wasm32-unknown-unknown --locked
```

The HTTP teammates conformance lane lives in `agentkit-http`:
`cargo test --locked -p agentkit-http --test http_teammates`.
The napi lane exposes 14 of the 15 Hybrid M1 teammate operations;
`streamAgentTask` (SSE) is intentionally absent on the native boundary
(see `crates/napi/src/teammate_lane.rs`).

Architecture rules (ADR 0021) are checked from the monorepo root with
`bun run check:arch`.

Environment: the release-projection and MCP stdio-peer suites spawn a real
`node` (Node 22 is the qualified version). There is no hosted CI: all gates run
locally, see [`DEVELOPING.md`](../DEVELOPING.md#local-gates-no-hosted-ci). The wasm check requires the
`wasm32-unknown-unknown` rustup target.

Mocked provider tests, loopback HTTP tests, local native builds, live provider
qualification, real keychain qualification and remote platform runs are separate
evidence. None implies another. No performance advantage is claimed.
