# Rust migration - review and hardening record

- Dates: initial record 2026-11-01 (pre-merge); revised same day after an external review at `24b3d17` which rejected several claims in the first version. This version supersedes it; the first version's "all backend execution is Rust / frozen TS has no production consumers / fresh-process qualification / Windows verified" claims were **overstated** — see "Claim corrections".
- Claim scope: Linux x64, Bun 1.4.4 (+ Node qualified cells). No live-provider, OAuth/keychain, macOS/Windows *execution*, or unavailable external integration results are claimed here. Windows lanes are compile/clippy-verified locally where toolchains allow; CI is the execution gate.
- Branch: `rust-migration`. The hardening tranche responding to the review (reproduced P1s: MCP write-handler gate bypass, expired queued callback dispatch, MCP post-cancel dispatch, NODE_OPTIONS startup-file approvals, restart continuation, post-admission `maxTokens` override; P1/P2 core hook/terminal parity; typed-install P2; CI lane failures; section-3 parity rows; plus the crash-adoption workspace-quarantine deadlock found during verification, in which an approved re-execution after a mid-command SIGKILL settled `completed` without executing — `docs/rust/parity.md` "coding-lane crash-recovery tranche") is recorded in `docs/rust/parity.md` / `acceptance.md` per slice.

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
| Composition ownership (structural, REQUIRED) | `bun run check:composition` — shipped compositions may not import backend implementations (CI lint job; scanner pinned by `scripts/tests/composition-ownership.test.ts`) |
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

## Independent re-verification (same day, after the tranche landed)

A second fresh-context pass re-ran the reviewer's own reproduction shapes
against the built addon rather than trusting the tranche's tests:

| Review item | Independent probe | Result |
| --- | --- | --- |
| P1 MCP write-gate duplicate registration | Same-name v1 read + v2 write, distinct counters, disjoint schemas, official SDK client, both orders, gate closed AND open | duplicate `registerTool` rejected `invalid_input`; only the accepted registration is advertised, schema-enforced and executable; gated write invisible and uncallable — 17/17 checks |
| P1 expired queued callback | 100 ms tool timeout, JS thread busy-blocked 1200 ms, out-of-process provider so the turn settles mid-block | ZERO entries into the abandoned callback after resume + double loop drain; control enters exactly once with a live signal |
| P1 post-admission `maxTokens` override | 128/16 admission with `providerConfig.maxTokens` unset / 8 / 1024, both protocols, plus the unsafe integer | wire `max_tokens`/`max_output_tokens` equals the `run.context.admitted` value in every case; 1024 clamps to 16; out-of-range refused at `createService` |
| P1 MCP cancel-before-dispatch (repo witness) | binding `mcp.test.ts` disk-log witness re-run | cancelled turn settled in ms; remote log after the FULL 2.5 s delayed-init window stayed `["echo","die"]` — zero late dispatch |
| P1 standing approvals / NODE_OPTIONS (repo native suite) | `command_authorization.rs` | 7/7 incl. `standing_approval_zero_execution_after_node_options_startup_file_mutation` (constant selector, mutated selected file → renewed approval, zero execution) and v3→v4 retirement |
| P2 typed installed consumer | `bun run check:native-package` (packed, isolated) | strict-tsc `NodeNext` consumer, `skipLibCheck` off: pass under bun and node; full acceptance lifecycle + fresh-process reopen pass |

Two gaps this pass found and closed: Responses terminal-reasoning parity
(terminal-only preservation, `stream_output_mismatch` conflict) had only
Rust-integration evidence — now pinned through the public binding in
`provider-lane.test.ts`; and the §6.6 structural composition gate did not
exist — now `scripts/check-composition-ownership.mjs`, required in the CI
lint job, with the scanner itself pinned by `scripts/tests/composition-
ownership.test.ts`. The `docs/rust/parity.md` ledger was corrected against
the code (host carve-out, `scripts/live` TS-pack drivers, new
`toolpacks-coding` row) and stale example-README wiring claims fixed.

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
