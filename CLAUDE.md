# AgentKit

Agent runtime library. The backend is Rust only (`rust/crates/`, a layered cargo
workspace). TypeScript (`packages/`, a bun workspace) is the API surface around
it: contracts, REST/SSE client, React hooks, the loop API and the Node-API
binding. OneMind (`../OneMind`) is the main consumer, so public API changes are
consumer-facing.

Start with `docs/architecture.md`. Decisions are in `docs/adr/`. Work items are
in the Plane project `AGENTKIT` (`AGENTKIT-<n>`).

## Layout

- `rust/`: cargo workspace, toolchain pinned in `rust-toolchain.toml`. Run cargo from `rust/`.
- `packages/`: `contracts`, `client`, `react`, `core`, `native` (plus per-platform addon packages), `testing`, and the `agentkit` umbrella.
- `scripts/`, `rust/scripts/`: gates, packaging and qualification. Run bun and node from the repo root.
- `docs/ports.md`, `docs/contracts.md` and `docs/rust/*.md` are large. Search them for the section you need instead of reading them whole.

## Crate layers

Dependencies point inward only. `LAYERS` and `CONTEXT_DEPS` in
`rust/scripts/check-arch.mjs` are the authority. If this table or a doc
disagrees with the script, the script wins.

| Layer | Crates (`agentkit-` prefix omitted) |
|---|---|
| 1 foundation | `kernel`, `contracts` |
| 2 execution-owner ports | `core` (model/tool loop), `native` (vendor runtime ports) |
| 3 contexts | `engine`, `governance`, `sessions`, `chat`, `agents`, `memory`, `native-exec` |
| 4 driven adapters | `store`, `providers`, `toolpacks-coding`, `toolpacks-web`, `lsp`, `mcp`, `connections`, `embed-local`, `sys`, `native-broker`, `claude` |
| 5 composition | `runtime` |
| 6 driving adapters | `napi`, `http`, `client`, `eval`, and the facade `agentkit-rust` (in `crates/agentkit`) |

`testkit` is a dev-dependency only.

Rules from ADR 0021, 0024, 0026 and 0027. `bun run check:arch` enforces the
dependency, `#[path]`, static and file-size rules. The rest are review rules.

- Layers 1–3 never depend on `axum`, `napi`, `rusqlite`, `reqwest`, `rmcp` or `keyring`.
- A context depends only on the contexts `CONTEXT_DEPS` lists for it. The chain is engine ← governance ← sessions ← chat ← agents. `memory` and `native-exec` may use engine and sessions only.
- Persistence goes through a repository port that the context declares. Only `runtime` and `napi` should name the concrete store. The allowlist still carries `chat`, `agents`, `governance` and `http`. Add no new uses. A new port gets an in-memory twin in `testkit` and one conformance suite that runs on both.
- Only `runtime` names concrete adapter types. It never depends on `claude` or `native-broker`. The consumer injects those.
- No `#[path]`, no glob re-exports at crate roots, no process-global mutable state (statics with interior mutability, `thread_local!`) outside `sys`. New platform-specific code and new `unsafe` go in `sys`.
- No non-test source file over 1,000 lines in layers 1–3 and 5. Split a file before it crosses the limit.
- Public error types are enums. A `String` is context, never the discriminant.

Each admitted turn has exactly one execution owner: the model loop (`core`,
`chat`) or a vendor runtime (`native`, `native-exec`, `native-broker`,
`claude`). Governed effects (staging, receipts, approvals) live in `governance`
and must work for both owners.

A new crate needs an entry in `LAYERS`, the workspace package fields, lints and
dependency pins, and a public-API baseline (`bun run update:rust-api`).

## Lists that only shrink

`rust/scripts/arch-allowlist.json` and `scripts/known-failures.json` record
existing violations and failing tests. Never add an entry to make a change
pass. Fix the cause, or stop and report it. Remove an entry when your change
resolves it, because the gates also fail on stale entries.

## Wire contracts

Rust is the wire authority (ADR 0022) and there is no code generator. A wire
change is three hand edits that must agree:

1. the DTO in `rust/crates/contracts/src/dto/`
2. `rust/crates/contracts/src/schema_bundle.json`
3. the TypeBox mirror in `packages/contracts/src/`

Then run `bun run contracts:check`. A change to exported TypeScript types also
changes the API reports: run `bun run build:contracts && bun run build:umbrella
&& bun run update:api`, review the diff in `docs/rust/api/*.api.md`, then run
`bun run check:api`. A new `run.warning` code also needs a row in
`docs/contracts.md`.

A new SQLite table or index is a `SCHEMA_VERSION` bump in
`rust/crates/store/src/schema.rs`, and the downgrade tests need updating with it.

## Verification

There is no hosted CI. A gate counts only when it ran on the tree you are
reporting. Say which gates ran and which did not.

- Full battery: `node scripts/gates.mjs`. Logs go to `.gate-logs/<step>.log`. It builds and tests everything, so run it before calling a change done, not in the edit loop. Subsets: `--only=fmt,clippy,test,arch`, `--skip=api`.
- While iterating on Rust, from `rust/`: `cargo test --locked -p agentkit-<crate>`, then `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`.
- After editing any `.rs` file, run `cargo fmt -p agentkit-<crate>` from `rust/` for each crate you touched. The global auto-format hook runs bare `rustfmt`, which ignores the workspace edition and leaves a style that fails the `fmt` gate.
- Other agents may be working in the same tree. Avoid `cargo fmt --all` and the `update:*` baseline scripts then, because they rewrite files in crates you did not touch. Stage only your own changes and re-check your edits before committing.
- While iterating on TypeScript: `bun run typecheck`, `bun run lint`, `bun test packages/<pkg>`.
- Order matters. The addon build, `bun run --cwd packages/native build:native`, comes before `bun run build`, `check:api` and `smoke:node`. The root `build:native` script is only the TypeScript build of that package and does not compile the addon. `bun run build:fixture` comes before the client and react tests. Native tests need `AGENTKIT_REQUIRE_NATIVE=1`, so a missing binding fails instead of skipping.
- `bun run check:rust-api` needs `cargo +nightly public-api`. Review a baseline diff before running `update:rust-api` or `update:api`.
- Tests marked `#[ignore = "live: ..."]` and the scripts in `scripts/live/` call real providers or a signed-in Claude CLI and spend tokens. Run them only when asked.
- `rust/target` grows to about 40 GB. `rust/target/debug/incremental` is safe to delete.

## Changes

- Commit subjects are `type(scope): summary`, where the scope is the crate or package (`fix(core): ...`).
- Commit moves and renames separately from behaviour changes.
- A change to a layer rule, a port's home or a wire guarantee needs an ADR with the next free number in `docs/adr/`.
- `docs/architecture.md` describes the tree as it is. Update it in the same change when crates, layers or ports move.
- Nothing tags, pushes or publishes by default. The scripts in `scripts/release/` run only on an explicit request.
