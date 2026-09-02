# Handoff — AgentKit hardening tranche 2

Session 2 · 2026-09-02

## Goal

Make AgentKit (contract 0.4.0 → 0.5.0) safe to migrate OpenPCB and OneMind onto: close the defects a six-reviewer
adversarial review found in the chat loop, tool calling, MCP client/server, durability layer and serving surfaces,
pin every fix with a test that fails on the old code, and leave the repo documented (ADR 0014) for the migration.

## Original plan

Session 1: `~/.claude/plans/act-as-senior-software-linked-seal.md` (findings, seven phases, delegation map, user
decisions, **Run log** with every commit and verification — append there). Session 2:
`~/.claude/plans/snappy-munching-wren.md` (waves A–E to finish the tranche: follow-ups, contract 0.5.0, docs,
crash/zombie E2E, verifier waves 2/3, Phase 6 splits).

User decisions (session 1): `chat_busy` default ON; file splits included LAST; harness `includeUserRequest` default
true; unattended per-phase execution (fable-run ritual). Session 2: release prep WITHOUT a tag (umbrella `0.5.0`,
`#v0.5.0` pins; the user tags/pushes per `DEVELOPING.md`); fold in the cheap follow-ups AND a sqlite schema v8;
shipped defaults confirmed as-is; Phase 6 = both splits.

## Done so far (and why)

Session 1 landed Phases 0–4, 5.2–5.4 and verifier wave 1 (see the session-1 handoff text preserved in the plan's
Run log). Session 2 landed everything that remained; master is 65 commits past `5cef867`, pushed to `origin/master` at the user's request at session end, no tag:

- **Contract 0.5.0 + release prep** — `CONTRACT_VERSION` 0.5.0; four Phase-2 golden scenarios (`tool-cap-run`,
  `duplicate-id-run`, `tool-timeout-run`, `unserializable-run`; `finishReason:"incomplete"` has no golden because
  it is SSE-client-produced); goldens re-recorded once (old traces moved only in `contractVersion`/`timestamp`/
  `eventId`); umbrella `packages/agentkit` at `0.5.0`, `#v0.5.0` install pins; `CHANGELOG.md` 0.5.0 entry.
- **sqlite schema v8** — `idx_messages_run(chat_id, run_id, depth, order_key)` (whole ORDER BY, no temp b-tree),
  `proposals.claimed_at`; `ProposalRecord`/`ProposalPatch`/`ProposalDto.claimedAt` stamped on the
  `approved → applying` claim and used by `reconcileInterrupted`'s window; `poisonCount` is now counted by the
  STORE on the `running → abandoned` edge of `endAttempt` (idempotent; first terminal status wins — a recovery
  pass can no longer restate a clean completion as a crash); `RunDto` still omits it (deliberate).
- **Crash/zombie E2E (5.6)** — `e2e vertical slice (D)` in `packages/runner-local/tests/e2e-vertical-slice.test.ts`:
  a provider parks ignoring `signal`, the clock passes the lease TTL, a SECOND runner + `TurnRunner` over the same
  store recovers onto attempt 2 (which chains from the tool record), and the zombie is refused both after attempt 2
  landed and while attempt 2 is still parked. Red-on-revert showed the success-path `transitionTask` fence is
  unreachable for a zombie (event appends are fenced first); the load-bearing fences are `appendEvents` and
  `failQuietly`'s transition.
- **Verifier wave 2** — 2a (host/adapters/runner): FIFO fairness was not true (`whenFree` now takes a real slot on
  the gate — a queued root write starved to the gate timeout under transaction load); `SqliteMcpServerConfigStore`
  joined strangers' transactions (now queues through a per-handle `writeGateFor(db)`); `endAttempt` first terminal
  wins; `openAgentKitDatabase` closes the handle on refusal; runner lands the fenced transition before `endAttempt`
  on every settle branch; `resetPass` clears `toolCallIds`; `failQuietly` finalizes the placeholder on either
  ownership proof. 2b (sse/client/react): an SSE pump exception now REJECTS the body (a clean EOF means "task
  terminal" and nothing else); hooks settle the phase from `RunStatusDto.status` when the stream closes without a
  terminal event; `submit`/`regenerate`/`refresh` carry the chat-switch guard; drain isolated from reconcile;
  `seq`-less events no longer disable dedupe; `finishReason` exposed on both hooks.
- **Docs (5.5)** — `docs/adr/0014-hardening-tranche-2.md` (15 decisions with rejected alternatives, both verifier
  waves recorded), `docs/contracts.md` (warning rows, `incomplete`, `run.failed.errorCode`, REST codes),
  `docs/roadmap.md` (Done + Later), both migration playbooks (`### Tranche 2 delta (0.5.0)`), `docs/ports.md`,
  `docs/architecture.md`, sqlite README.
- **Verifier wave 3** — pure-move verdict clean for both splits; six small findings fixed inline (host barrel
  no longer leaks turn internals; conformance pins fence-before-short-circuit; `settleResolved` logs an
  `endAttempt` failure after landing; fake returns copies; slice D proves the zombie resumed).
- **Phase 6** — pure-move splits: `turn-runner.ts` 2367 → 1368 (`turn/history-assembly.ts`, `submit.ts`,
  `harness-driver.ts`, internal `pass-types.ts`); `sqlite-assistant-store.ts` 3819 → 428 (`src/sqlite/*`). Test
  totals unchanged; public surface unchanged.

Dead-ends / do-not-repeat (both sessions): skipping `this.active` in recovery; `ready()`+`withTx` split; pure
id-keyed accumulators; golden drivers in `packages/testing/src`; sideways `normalizeServerAlias` import; a
`txDepth`/`db.inTransaction` check as an ownership test; re-reading the gate owner instead of taking a slot;
`git stash` in agent worktrees (shared `refs/stash` — use patch files); `HangingProviderClient` for crash sims
(it only ends by abort — park on a promise that ignores `signal`).

## How to resume

1. Run the `handoff` skill with "resume". Read `HANDOFF.md` → `CURRENT_STATE.md` → `TODO.md`, then the plan files'
   Run log, then memory `agentkit-hardening-tranche-2`.
2. Verify: `bun install --frozen-lockfile && bun run typecheck && bun test && bun run lint && bun run build && bun run build:umbrella && bun run smoke:umbrella && node scripts/node-smoke.mjs` — expect the numbers in `CURRENT_STATE.md`.
3. Next: the tranche is COMPLETE (waves A–E, verifier waves 1–3 all closed; see the Run log). The user's call:
   cut `v0.5.0` (`DEVELOPING.md` "Releasing": tag `v0.5.0` on master and push — CI builds the umbrella release
   branch; master is pushed, the tag is not), or start the OpenPCB migration (`docs/migration/openpcb.md`, §8
   Sequencing), or P6 (`docs/roadmap.md`).
4. Ritual unchanged: delegate ≥150-line tasks to worktree agents with a brief file; red-on-revert via patch file;
   merge with `git -C <abs repo>`; run the full gate yourself; commit per landing; never push; append to the Run log.

## Open questions

- Cut the `v0.5.0` tag now? Everything is prepared; only the user tags/pushes.
- Roadmap Later items surfaced this session that may deserve a small tranche before the migration: the one-write
  `retry_pass` misreport window (host-side batching), `sse.ts` `roomAvailable()` idle deadline, `AbortSignal` on hook
  ports, `TurnRunner`'s unfenced terminal `updateMessage` (needs a lease-aware `ConversationStore` write),
  `stillHoldsLease` renewing as a side effect, `deadLetter()`'s internal fence order.

## Pointers

- Tasks → TODO.md · Snapshot → CURRENT_STATE.md · Plans + Run log → `~/.claude/plans/act-as-senior-software-linked-seal.md`, `~/.claude/plans/snappy-munching-wren.md` · Memory → `~/.claude/projects/-Users-andrejvysny-workspace-openpcb-AgentKit/memory/agentkit-hardening-tranche-2.md`
