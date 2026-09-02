# TODO — hardening tranche 2 (plan: ~/.claude/plans/act-as-senior-software-linked-seal.md, ADR 0014, contract 0.4.0 → 0.5.0)

Updated: 2026-09-02 (session 2)

## Now (plan: ~/.claude/plans/snappy-munching-wren.md — waves A→E) — TRANCHE COMPLETE

- [ ] user decision: cut `v0.5.0` (tag + push per DEVELOPING.md) — or start the OpenPCB migration / P6

- [x] Wave A — A1 inline: `errorCode` on 2 `run.failed` sites (core provider) + flaky e2e `waitFor` budgets (react/client)
- [x] Wave A — A2 (impl-critical): sqlite `SCHEMA_VERSION` 7→8 (`idx_messages_run`, `proposals.claimed_at`), `ProposalRecord.claimedAt`, store-side `poisonCount` on `endAttempt(abandoned)`, conformance ×2 adapters
- [x] Wave A — A3 (impl-critical): 5.6 E2E — crash after internal assistant record → recover → attempt 2 on active path; zombie cannot land terminal
- [x] Wave B — 5.1: `CONTRACT_VERSION` 0.5.0, 4 Phase-2 golden scenarios, re-record goldens once, release prep (umbrella 0.5.0, `#v0.5.0` snippets), CHANGELOG 0.5.0
- [x] Wave C — 5.5 docs (ADR 0014, contracts.md, roadmap, migration deltas, ports/architecture, sqlite README) ∥ verifier wave 2 (host/adapters/runner + 4.1 client/react); fix findings
- [x] Wave D — Phase 6: `turn-runner.ts` split ∥ `sqlite-assistant-store.ts` split (pure moves)
- [x] Wave E — verifier wave 3 (pure-move check + A2/A3/B), final gate, handoff files + memory + Run log

## Phase 0 — stop the bleeding
- [x] 0.1 write-policy body `chatId` override (B1) — inline
- [x] 0.2 sqlite/memory transaction owner gate (D1, D2)
- [x] 0.3 transaction concurrency tests

## Phase 1 — durability + fencing
- [x] 1.1 fenced terminal writes (C3/D4, D9) — ports + adapters + runner + TurnRunner reorder + conformance
- [x] 1.2 `availableAt` normalization (D3)
- [x] 1.3 memory adapter parity (D5, D6)
- [x] 1.4 runner hygiene (D7, D8, D10, D11)
- [x] 1.5 outbox bounds (D12)

## Phase 2 — core chat loop
- [x] 2.1 cancellation truth (A1, A6, A15)
- [x] 2.2 tool-call assembly (A4, A8, A7, A12)
- [x] 2.3 tool execution safety (A2, A3, A10, A14, default tool timeout)
- [x] 2.4 provider client bounds (A5, A11, A13)
- [x] 2.5 Ajv hardening (A9, F6)
- [x] 2.6 goldens replayed live for all 5 scenarios; re-record once (E3)

## Phase 3 — host orchestration
- [x] 3.1 recovery chain resume (C1)
- [x] 3.2 throw path finalize + run.failed/cancelled (C2, C12, C7, C10)
- [x] 3.3 pass-boundary `retry_pass` warning (F-OWN-1 host half)
- [x] 3.4 submit exclusivity `chat_busy` default ON (C5)
- [x] 3.5 hook deadlines (C6)
- [x] 3.6 proposals: scopeKey allowance, fixed strings, reconcile guard, key escape (C4, C8, C9, C14)
- [x] 3.7 emulated-call detector (C11)
- [x] 3.8 projection order (C13)
- [x] 3.9 delta write coalescing (F-OWN-4)
- [x] 3.10 harness `includeUserRequest` (F-OWN-5)

## Phase 4 — serving surfaces
- [x] 4.1 stream close rule + runPhase last-terminal + react reset (F-OWN-1)
- [x] 4.2 react hooks (B2, B8, B9, B12)
- [x] 4.3 client resume (B5)
- [x] 4.4 transport bounds + validation (B3, B4, B6, B10, B11, B13, B14, B15, E2, E6, F5)
- [x] 4.5 authz / privileged-resource docs (B7, F5, F14)
- [x] 4.6 MCP server (F1, F2, F3, F7, F9, F13)
- [x] 4.7 MCP client (F4, F10, F11, F12, F8)

## Phase 5 — contracts, testing infra, packaging, docs
- [x] 5.1 contract 0.5.0 (E1, E11, E13 done; new codes, chat_busy, fenced options); CONTRACT_VERSION bump
- [x] 5.2 SecretStore conformance + reference impl (E5)
- [x] 5.3 CI (E4, E9, E7)
- [x] 5.4 packaging (E8, E10, E12)
- [x] 5.5 docs: ADR 0014, architecture/ports/contracts, CHANGELOG, roadmap, migration delta
- [x] 5.6 second-attempt + zombie E2E

## Phase 6 — structure (last)
- [x] turn-runner.ts split (2367 → 1368 lines; history-assembly / submit / harness-driver / pass-types)
- [x] sqlite-assistant-store.ts split (3819 → 428 lines; sqlite/{connection,rows,conversation-store,task-store,proposal-store,provider-store,settings-store,outbox-store})

## Verifier passes (fresh-context `reviewer-critical`, read-only, 2 agents per wave)
- [x] wave 1 — Phase 0/2/4 + 5.2–5.4 (25 findings, all closed: V1–V14, W1–W11)
- [x] wave 2 — Phase 1 (fencing), Phase 3 (host), adapters V7–V9, 4.1 multi-pass (2a: 7 findings, 2b: 9 findings — all HIGH/MED fixed; F6/F9 of 2b deferred)
- [x] wave 3 — after Phase 5–6 (pure-move verdict clean; 6 findings fixed inline: barrel leak, fence-first conformance case, settleResolved endAttempt guard, fake returns copies, zombie-resumed assertion)

## Follow-ups surfaced by agents/verifiers (not in plan; decide before or after 5.5)
- [x] `poisonCount` exact only if incremented on `endAttempt(abandoned)` store-side
- [x] `idx_messages_run ON messages(chat_id, run_id, depth, order_key)` + `ProposalRecord.claimedAt` — sqlite schema v8
- [x] two other `run.failed` sites in `openai-compatible.ts` still lack `errorCode`
- [x] `docs/contracts.md` warning table lacks Phase-2 codes (folds into 5.5)
- [ ] memory adapter does NOT queue ordinary writes behind an open tx (documented adapter-MAY); full parity needs sub-store state injection
- [x] react/client real-socket e2e tests flake under concurrent test load (~1 s `waitFor`); raise budgets or serialize
- [ ] `useChat.error` stale until reconcile during pass 2; failed submit clears run fields of a concurrently accepted run
- [ ] MCP `toolAliases` values ungrammatical at REST boundary; `withHookDeadline` cannot cancel hooks (needs AbortSignal on hook ports)
- [x] no golden scenario covers Phase-2 paths (byte cap, dup ids, timeout, `incomplete`)
- [ ] `resume.test.ts` now ~2.8 s (retry floor 250 ms)
