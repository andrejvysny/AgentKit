# TODO — hardening tranche 2 (plan: ~/.claude/plans/act-as-senior-software-linked-seal.md, ADR 0014, contract 0.4.0 → 0.5.0)

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
- [ ] 5.1 contract 0.5.0 (E1, E11, E13 done; new codes, chat_busy, fenced options); CONTRACT_VERSION bump
- [x] 5.2 SecretStore conformance + reference impl (E5)
- [x] 5.3 CI (E4, E9, E7)
- [x] 5.4 packaging (E8, E10, E12)
- [ ] 5.5 docs: ADR 0014, architecture/ports/contracts, CHANGELOG, roadmap, migration delta
- [ ] 5.6 second-attempt + zombie E2E

## Phase 6 — structure (last)
- [ ] turn-runner.ts split
- [ ] sqlite-assistant-store.ts split

## Verifier passes
- [ ] after Phase 0–2 wave
- [ ] after Phase 3–4 wave
- [ ] after Phase 5–6
