# ADR 0021: Layered crate architecture and dependency rules

Status: accepted (2026-11-09). Supersedes the crate layout described in
`docs/architecture.md` before this date.

## Context

The Rust engine grew around one crate. `agentkit-host` reached 60k lines
(tests included) and owns task execution, chat, sessions, subagents, advisor,
teammates, governance, MCP configuration and the HTTP backend. Its root
re-exports every module, modules reach each other through the root, and
`#[path]` attributes hide the real tree. `agentkit-store` exposes one concrete
`SqliteStore` with no traits, and domain rules live inside it. The only
composition root is the Node binding. Hybrid M2–M6 would add another large
body of code to the same crate.

## Decision

The workspace is organised in six layers. Dependencies point inward only.

1. Foundation: `agentkit-kernel` (hashing, canonical JSON, clock and id
   ports, shared primitives) and `agentkit-contracts` (wire DTOs and schema
   validation).
2. Agent loop and governance: `agentkit-core` (model/tool loop and its ports)
   and `agentkit-governance` (tool guards, approvals, execution authority,
   receipts, proposals).
3. Application contexts, each owning its repository traits:
   `agentkit-engine` ← `agentkit-sessions` ← `agentkit-chat` ←
   `agentkit-agents`. New Hybrid contexts (memory, automation, remote) depend
   on engine and sessions only.
4. Driven adapters: `agentkit-store-sqlite`, `agentkit-store-legacy-import`,
   `agentkit-providers`, `agentkit-toolpacks-coding`, `agentkit-lsp`,
   `agentkit-mcp`, `agentkit-connections`, `agentkit-sys`.
5. Composition: `agentkit-runtime` with `AgentRuntime::builder()`. It is the
   only crate that names concrete adapter types.
6. Driving adapters: `agentkit-napi`, `agentkit-http`, `agentkit-client` and
   the `agentkit` facade.

`agentkit-testkit` is a dev-only crate with shared fakes, in-memory
repositories and repository conformance suites.

`agentkit-host` is removed once its modules have moved. No re-export shim is
kept: the Rust crates are not published and every consumer lives in this
repository.

## Rules

Enforced by `rust/scripts/check-arch.mjs` over `cargo metadata`, cargo-deny
and code review.

- R1. Layers 1–3 do not depend on `axum`, `napi`, `rusqlite`, `reqwest`,
  `rmcp` or `keyring`.
- R2. Contexts depend only leftward in the chain engine ← sessions ← chat ←
  agents.
- R3. Driven adapters do not depend on the runtime or on driving adapters.
- R4. `agentkit-contracts` is used inside layers 2–3 only for persisted event
  envelopes; DTO to domain mapping happens in the driving adapters.
- R5. No `pub use x::*` at crate roots, no `#[path]`, no mutable statics
  outside `agentkit-sys`.
- R6. Every crate inherits `[workspace.package]`, `[workspace.lints]` and
  workspace dependency versions, including path dependencies.
- R7. Public error types are enums; a `String` payload is context, never the
  discriminant.
- R8. Every `#[napi]` export runs under `catch_unwind` and decodes its input
  through a derived DTO.
- R9. Platform-specific code lives in `agentkit-sys`; Windows compiles and
  refuses explicitly where a capability is Unix-only.
- R10. Test fakes come from `agentkit-testkit`; each repository trait has one
  conformance suite run against SQLite and in-memory implementations.

Existing violations are listed in the check script's allowlist. The allowlist
only shrinks; each migration phase removes the entries it resolves.

## Consequences

- Hybrid features land in their own crates instead of growing host.
- Rust embedders (egui, Tauri) use the same builder as the Node binding.
- More crates mean more manifests and link units; heavy dependencies stay in
  leaf adapters to keep incremental builds fast.
- Persistence becomes replaceable per context, and in-memory repositories make
  domain tests independent of SQLite.

## Migration

Phases P0–P8 are tracked in Plane. Each extraction lands as a pure-move commit
(unchanged test totals and behaviour) followed by separate behaviour commits.

## Amendment (2026-11-11)

- Layer 2 gains `agentkit-native`: ports and contracts for vendor agent runtimes (ADR 0025).
- Layer 3 gains `agentkit-native-exec`. Like the future Hybrid contexts it may depend on engine and
  sessions only.
- Layer 4 gains `agentkit-native-broker` and `agentkit-claude`. `agentkit-store` keeps its name;
  `agentkit-store-sqlite` and `agentkit-store-legacy-import` will not be created (ADR 0024).
- Layer 6 temporarily holds `agentkit-facade`.
- The allowlist only shrinks, with one exception: a merge from `master` may add the entries its
  commit message names. The merge of 2026-11 added four; phases N1 and G1 remove them.
