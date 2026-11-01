# Rust migration - review and hardening record

- Dates: initial record 2026-11-01 (pre-merge); revised same day after an external review at `24b3d17` which rejected several claims in the first version. This version supersedes it; the first version's "all backend execution is Rust / frozen TS has no production consumers / fresh-process qualification / Windows verified" claims were **overstated** — see "Claim corrections".
- Claim scope: Linux x64, Bun 1.4.4 (+ Node qualified cells). No live-provider, OAuth/keychain, macOS/Windows *execution*, or unavailable external integration results are claimed here. Windows lanes are compile/clippy-verified locally where toolchains allow; CI is the execution gate.
- Branch: `rust-migration`. The hardening tranche responding to the review (reproduced P1s: MCP write-handler gate bypass, expired queued callback dispatch, MCP post-cancel dispatch, NODE_OPTIONS startup-file approvals, restart continuation, post-admission `maxTokens` override; P1/P2 core hook/terminal parity; typed-install P2; CI lane failures; section-3 parity rows) is recorded in `docs/rust/parity.md` / `acceptance.md` per slice.

## Claim corrections (against the first version of this record)

| First-version claim | Corrected status |
|---|---|
| "The entire backend runs on Rust... zero production consumers" | Was true only for loop/store/host/session lanes. Coding pack executed TS tool bodies through the example until this tranche's `registerCodingTools`; desktop managed-integrations, `node-host.mjs`, and the OpenPCB smoke still ran TS backend implementations at review time — their status is tracked in the tranche entries, not claimed here. |
| "Binding suite... MCP... green" as binding-level assurance | The binding paths bypassed crate-level guarantees (write gate vs executed callback, unbound dispatch, cancel-after-settle). Binding-level pins now exist (`mcp.test.ts`, `tool-identity`, `invocation-liveness`), which is what the first version should have meant. |
| "Foreign targets: clean" | Windows cells FAILED CI at `24b3d17` (E0658 unstable `windows_by_handle` in the project-instructions reader). Fixed in the tranche via a stable-API identity path; CI remains the execution gate. "Compile/semantics verified" was honest; "clean" was not. |
| "responses-smoke bridge-free exit 0" implying restart proof | The old smoke proved store-reopen under one process-global connection, NOT application restart. A real two-process continuation test now exists (`provider-continuity.test.ts`) and the smoke has a genuine two-process phase. |

## What the branch delivers (verified)

Backend logic in Rust (`rust/crates/*`) behind the frozen public TypeScript API via NAPI-RS bindings (`packages/native`): core loop incl. hooks/deadlines/terminal/media parity pins, store incl. receipts/conformance-parity, host session/compaction/permissions/leases, two-protocol provider lane with admission-consistent request building, durable logical provider connections surviving restart, MCP client+server with identity/authority invariants pinned at the binding, native coding toolpack with standing approvals (v4 fingerprints), native HTTP server incl. IPv6. Client-side TS/React preserved. Frozen TS backend libraries remain only as API-freeze + differential-test assets.

## Review map

| Area | Start here |
|---|---|
| Per-package production role + per-slice fix ledger | `docs/rust/parity.md` |
| Verification ledger (conformance, binding surface, per-tranche evidence) | `docs/rust/acceptance.md` |
| Binding milestone + documented reductions | `docs/rust/native-binding-milestone.md` |
| Public-API freeze | `bun run check:api` (31 reports) |
| Changelog | `CHANGELOG.md` Unreleased |

## Verification gates (re-run all before merge)

| Gate | Command |
|---|---|
| Rust | `cd rust && cargo test --workspace --locked`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all --check` |
| Windows compile | `cargo clippy -p agentkit-host --target x86_64-pc-windows-msvc -- -D warnings` (CI `native-windows`) |
| TS suites | root `bun test`; `cd packages/native && bun test tests` |
| Typecheck | root typecheck incl. examples + native tests |
| API/parity | `bun run check:api`; `bun run check:parity` |
| Fingerprint differential (REQUIRED) | `AGENTKIT_RUN_RUST_CONFORMANCE=1 bun test packages/toolpacks-coding/tests/command-fingerprint-rust.test.ts` + named CI step |
| Acceptance | `bun scripts/qualification/responses-smoke.mjs <db>` (two-process; fresh db path; Bun only) |
| Installed artifacts | pack + strict-typed consumer + runtime smoke (bun/node) via the qualify scripts / CI pack lanes |

Rebuild note: `cd packages/native && bun run build:native` is the canonical binding build (root `build` is tsc-only).

## Known limitations (honest, as of this tranche)

- MCP binding: no OAuth/elicitation, deny-all `secretRefs`, in-memory configs (reductions documented in the milestone doc).
- Store scheduler priority aging: the 2 binding-suite skips.
- Node beyond qualified cells, live providers, macOS/Windows execution: untested here; CI/state as above.
- Frozen TS backend packages remain published (API-freeze constraint); production consumers are removed per-slice — see parity ledger for the final consumer state, NOT this summary.

## Merge readiness checklist

- [ ] Tranche merged/pushed; CI green on push for the declared release scope (the one gate this machine cannot run)
- [x] All gates green locally on the final tree (matrix below)
- [x] Reviewer-reproduced P1s closed through the public bindings with permanent boundary tests
- [x] This record corrected against the first version's overclaims
- [ ] Maintainer: tag/release decision per `DEVELOPMENT.md`

## Verification matrix (final integration chain, single quiet-machine sequential run)

| Gate | Result |
|---|---|
| `cargo fmt --all --check` (rust) | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | NO-FAILURES across all crates |
| Windows cross-clippy (local) | ENVIRONMENT BLOCK: dies in dependency build scripts (`cc-rs`: `lib.exe` not found; `aws-lc-sys`, `libsqlite3-sys`); zero rustc/clippy errors reached any `agentkit` crate. CI `native-windows` (windows-latest, in `rust.yml`) is the execution gate — runs on push |
| Root `bun test` | 4009 pass / 0 fail / 42 skip |
| Native binding suite (`packages/native`, Bun) | 270 pass / 0 fail / 2 skip (skips = priority-aging, documented) |
| Root typecheck (all packages + examples + native tests) | 0 errors |
| `bun run check:api` (public-API freeze) | 31 reports match |
| `bun run check:parity` | pass |
| Fingerprint differential (`AGENTKIT_RUN_RUST_CONFORMANCE=1`) | pass |
| `bun run check:native-package` (staged `npm pack` + platform tarball + strict typed consumer) | pass |
| `scripts/node-smoke.mjs` (Node 22 cell) | exit 0 |
| `scripts/umbrella-smoke.mjs` | exit 0 |
| `scripts/qualification/responses-openpcb-smoke.mjs` (Node) | exit 0 |
| `scripts/qualification/responses-smoke.mjs` (two-process; fresh DB) | exit 0 under Bun and under Node |
