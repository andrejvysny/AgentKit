# Plane paste-in: rust-migration TS deletion program (2026-11-02)

No Plane API key / MCP server in this environment; these are the exact bodies to
create/update. Project prefix `AGENTKIT`.

---

## UPDATE AGENTKIT-58 — "Replace TypeScript backend with Rust native stack"

Add status comment:

> Phase 0 (Rust hardening) complete 2026-11-02: PID-reuse-safe kills, lock-poison
> recovery, error redaction in the NAPI boundary, explicit terminal wire strings,
> rationaled `#[ignore]`s, hand-formatted provider macro bodies, coordination-dir
> permission hardening, corrected milestone/parity docs. Phase 1 deletes
> `@agentkit/mcp-server`; Phase 2 deletes `@agentkit/toolpacks-coding` after
> test-mapping gates. Deletion criterion: behavior parity (API may break);
> packages with Rust gaps stay TS until the linked blocker items below close.

Linked blockers to attach: AK-58a…AK-58f (kept packages), AK-P0-1…AK-P0-8 (done).

---

## Kept-package blockers (status: In Progress / Open)

### AK-58a — transport-http: close native gap, then delete TS (5.6k LOC)
- Connector lifecycle routes return 501 in the native REST server; TS
  `packages/transport-http` owns connector lifecycle.
- Provider secrets are memory-only in the native path; TS has durable
  provider-secret handling. Needs a host-side SecretStore port decision
  (OS keychain) before parity.
- Exit: native connector lifecycle implemented + persistent secret store,
  consumers (AGENTKIT-56) moved; then delete package + umbrella/smoke/API refs.

### AK-58b — runner-local: priority aging + worker export (2.1k LOC)
- Rust scheduler lacks TS priority aging.
- Worker entry not exported through `@agentkit/native`.
- Exit: aging implemented + worker exported natively; retarget consumers; delete.

### AK-58c — adapters-sqlite / adapters-memory: native store surface (9.3k LOC)
- No native SecretStore, MCP config store, MCP connection store; live in-memory
  Maps for those namespaces remain TS-only.
- Exit: ports implemented in Rust adapters + conformance parity; delete packages.

### AK-58d — mcp-client: OAuth, elicitation, config persistence, rich content (4.3k LOC)
- Rust client lane lacks OAuth flows, elicitation handling, config persistence,
  rich content mapping.
- Exit: gaps closed in `rust/crates/mcp` client + interop suites; delete package.

### AK-58e — host: export services natively; profiles/descriptors (19.6k LOC)
- Host services not exported through the binding; profile/descriptor subsystem
  TS-only.
- Exit: native exports + profile/descriptor port; delete package.

### AK-58f — core providers/tools: JS provider lane + Responses deviation (4.7k LOC)
- `AiProviderClient` JS lane is public API; cannot delete while JS consumers
  inject providers.
- Responses provider has an accepted `malformed_usage` deviation from TS.
- Exit: deviation resolved or documented as permanent; JS lane decision; delete.

---

## Phase 0 findings (status: Done 2026-11-02 — record-keeping)

- **AK-P0-1** PID-reuse-safe kill: `git_tool.rs` (2 sites), `process.rs` kill
  paths now kill via owned child/group handles (`Child::start_kill`, group
  leader probe); SAFETY comments on remaining `unsafe` (`command_fingerprint.rs`,
  `coding_lane.rs`).
- **AK-P0-2** Lock-poison recovery (`PoisonError::into_inner`) in
  `mcp/src/manager/session.rs` and `core/src/budget.rs`.
- **AK-P0-3** `napi/src/errors.rs`: unknown/internal codes → generic message;
  raw message never crosses to JS; `error-codes.json` parsed once (OnceLock).
- **AK-P0-4** `napi/src/run.rs`: terminal wire string from explicit match, not
  `Debug` formatting.
- **AK-P0-5** All 8 bare `#[ignore]`s now carry reasons; all are
  `--ignored --exact` fixtures (load-bearing), not skipped tests.
- **AK-P0-6** Provider `stream!` macro bodies hand-formatted in
  `providers/{responses,chat_completions}.rs` (rustfmt-invisible minified bodies).
- **AK-P0-7** Coordination lockdir: owner uid + mode check enforced
  (`verify_private_dir`, tightened against group/world-writable incl. hosts
  with umask 002); non-Unix path fixed.
- **AK-P0-8** Docs: `native-binding-milestone.md` windows row corrected (binding
  DOES depend on `agentkit-toolpacks-coding`); `parity.md:516` phase-1 matrix
  marked superseded by the dated ledger.

---

## Phase 1 (status: In Progress)

### AK-P1 — Delete `@agentkit/mcp-server` (2.4k LOC)
- Zero importers; native `startMcpServer` exists.
- Gate: 11 TS test files mapped to native/Rust coverage
  (`local://mcp-server-test-map.md`); uncovered cases (Host/Origin guard,
  bearer, principal sessions, prompts/resources, request cancellation,
  inbound actors) ported or explicitly dropped.
- Removal: package, umbrella subpath + specifiers, pack/node/umbrella smokes,
  composition-ownership BACKEND set, qualification/types, root scripts,
  API reports + manifest, README refs, `host/src/ports/tool-guard.ts:43` comment.

## Phase 2 (status: Open)

### AK-P2 — Delete `@agentkit/toolpacks-coding` (9.5k LOC)
- Gate: 27 TS test files mapped (`local://toolpacks-coding-test-map.md`);
  fingerprint differential frozen as fixtures under
  `rust/fixtures/coding-fingerprint/`; CI differential step replaced by fixture
  test.
- Consumers retargeted to `registerCodingTools` (native tests, coding-agent
  example, live session scripts).
- Removal checklist as AK-P1, plus HARD_BANNED scanner cleanup and host
  comments (`plan-mode.ts:189`, `agent-profile.ts:103`).
