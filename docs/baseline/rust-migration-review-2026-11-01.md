# Rust migration - final pre-merge review record

- Date: 2026-11-01
- Branch: `rust-migration` @ merge commit `26bc6d0` (Merge branch `rust-migration-t1`, external-review tranche W4-W11)
- Review diff for the tranche: `82ad438..26bc6d0` (46 files, +5690/-212); full branch diff vs `master` is the whole migration.
- Claim scope: Linux x64, Bun 1.4.4, Node v22.20.0 qualified cells. No live-provider or unavailable external integration results are claimed.

## What the branch delivers

The entire backend runs on Rust (`rust/crates/*`) behind the frozen public TypeScript API, exposed through NAPI-RS bindings (`packages/native`). Client-side TS/React (`client`, `react`, `contracts`) is preserved; frozen TS backend libraries remain only as API-freeze artifacts with zero production consumers. Compositions: both examples, all four live scripts, and the qualification smoke run on `@agentkit/native`.

## Review map

| Area | Start here |
|---|---|
| Surface ledger (per package: production role, test-only consumers) | `docs/rust/parity.md` |
| Verification ledger (engine conformance, binding runtime surface, JS-tool guard/proposal-apply rows) | `docs/rust/acceptance.md` |
| Binding milestone + reductions (MCP OAuth/elicitation, CORS, provider-write routes) | `docs/rust/native-binding-milestone.md` |
| Public-API freeze reports | `docs/rust/api/*.api.md` (`bun run check:api`) |
| Changelog narrative | `CHANGELOG.md` (Unreleased) |
| Review-tranche commits | `git log 82ad438..26bc6d0 --oneline` |

## Review focus (highest-risk changes in the tranche)

1. **Proposal finalization** - `rust/crates/store/src/proposals.rs`: terminal stamp derives only from the committed receipt (first-write-wins); no-evidence plain CAS refused. Pinned by store unit tests + fresh-process reopen tests + binding-level port projections (`packages/native/tests/proposals.test.ts`).
2. **MCP identity/dispatch** - `rust/crates/mcp/src/manager/*`, `rust/crates/napi/src/mcp_client.rs`: advertised==executed catalogue, one-step alias gate, `call_bound` goes through `request()` with a handle-pinned guard and wire-last cancellation check. Tests: `rust/crates/mcp/tests/generation.rs`, `packages/native/tests/mcp.test.ts` (real SDK client).
3. **Cancellation bridge** - `rust/crates/napi/src/jsbridge.rs` + `packages/native/src/callback-router.ts`: host cancel reaches JS callbacks (`signal.aborted` observed at ~4 ms); cancelled coding runs take their process group down. Tests: `packages/native/tests/cancellation.test.ts`.
4. **Approval fingerprint agreement** - `rust/crates/toolpacks-coding/src/command_fingerprint.rs` vs `packages/toolpacks-coding/src/command-fingerprint.ts`: shared tokenizer fixture (`rust/fixtures/coding-fingerprint/tokenizer.json`) run from both sides; effective-env claim revalidation; envelope v3. Gate name: `fingerprint-conformance` in CI.
5. **Windows paths / instructions** - `rust/crates/host/src/project_instructions/fs.rs`, `packages/native/src/assistant-store.ts` (`fileURLToPath`): compile/semantics verified here; execution CI runs in the `native-windows` MSVC job (this host is Linux).

## Verification matrix (observed on the pre-merge tree `1b00d15`, merged byte-identically)

| Gate | Command | Result |
|---|---|---|
| Rust tests | `cargo test --workspace --locked` | 0 failures (incl. store proposals, host 223, mcp incl. `generation`, napi) |
| Rust lints | `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all --check` | clean |
| Foreign targets | FingerprintGuard 3-target clippy gate; CI `native-windows` (MSVC) | clean / branch-compiling |
| Root suite | `bun test` | 3921 tests / 0 fail / 36 skip (273 files) |
| Binding suite | `cd packages/native && bun test tests` | 199 pass / 0 fail / 2 skip (documented no-priority-aging cases) |
| Typecheck | root `tsc` across packages + examples + native tests | clean |
| API freeze | `bun run build && bun run build:umbrella && bun run check:api` | exit 0, 31 reports match |
| Parity | `bun run check:parity` | exit 0 (transport-http 223, mcp-server 112, mcp-client 100, 192, 1034) |
| End-to-end acceptance | `bun scripts/qualification/responses-smoke.mjs <db>` (bridge-free) | exit 0 - `{requests:3, toolExecutions:1, privateContinuationReopened:true, publicLeak:false}` |

Notes for re-running: build the binding first (`bun run build:native` in `packages/native`; root `build` only runs `tsc`). The smoke takes the DB path as a positional argument and must run under Bun (`Bun.serve`); remove a stale DB file before a re-run.

## Known limitations (documented, non-blocking)

- Frozen TS backend packages stay as exports - removal is an approval-gated breaking change; production has zero consumers (ledger in `parity.md`).
- MCP binding: no OAuth/elicitation, deny-all `secretRefs`, in-memory configs; HTTP server: CORS + provider-write routes unwired in `startHttpServer`; store scheduler: priority aging skipped in 2 conformance cases (the 2 binding-suite skips).
- Live-provider qualification and the two MANUAL live smokes (`coding-session`, `approval-session`, `compaction-session`) need a real model endpoint; not executed or claimed in this record.
- Node support beyond the qualified cells is untested.

## Merge readiness checklist

- [x] Tranche fully merged (`26bc6d0`), tree clean, no unpushed commits after push
- [x] All gates green on the merged content (matrix above)
- [x] API freeze + parity untouched or intentionally updated (no report deltas in tranche)
- [x] Docs updated: `parity.md` ledger + engine-fix entries, `acceptance.md` rows, CHANGELOG Unreleased
- [x] No TODO/PLAN scaffolding files; no `TODO: implement` markers in tranche diff
- [ ] Maintainer: CI green on push (local-only policy prevented remote CI runs)
- [ ] Maintainer: tag/release decision per `DEVELOPING.md`
