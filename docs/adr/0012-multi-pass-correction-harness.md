# ADR 0012 — Multi-pass correction harness over `VerificationHook`

**Status:** accepted, implemented (2026-09-01; provider-legal replay
ordering fixed 2026-09-02)
**Contract impact:** Additive under `CONTRACT_VERSION` `0.4.0` — new event
type `run.verification` (the 13th member of the vocabulary) and a new
optional `TurnRunnerDeps.correction` dependency; no version bump per policy.

## Problem

`VerificationHook` already existed and `TurnRunner` already invoked it
exactly once, after any run that made tool calls — but nothing fed a
resulting `DeficiencyReport` back to the model. [`docs/non-goals.md`](../non-goals.md)
named this explicitly: "a single verification invocation is implemented; the
harness that would loop on it, with its own cost and stopping condition, is
future work." OpenPCB's own assistant already runs this loop in production
and needs parity for the migration this framework exists to enable.

## Evidence

OpenPCB `run-service.ts:275,397-494`'s `runCorrectionHarness` — minimal
re-context (not full history), a shrink-or-stall stopping rule on the
failing-check set, a max-pass cap, fail-closed checks ("verification
unavailable is not a pass"), a deficiency write-back message — all named
already in [`docs/roadmap.md`](../roadmap.md)'s prior Later entry for this
phase, before it was built, and preserved here rather than redesigned.

A Phase B/C fresh-context verifier pass, run the day after landing, found a
**CRITICAL**: a tool-calling correction pass reuses the *same* run id and
therefore produces a **second** assistant turn carrying `tool_calls` on that
run — which broke `orderMessagesForProvider`'s existing per-**kind** bucketing
(every internal assistant turn ahead of every tool result). Replaying two
tool-calling turns back to back, with the first left unanswered until after
the second, is something every provider rejects outright. The correction
harness was the first thing in the codebase to ever produce more than one
tool-calling pass on a single run id, so it was also the first thing to
exercise this latent defect.

## Decision

1. **Opt-in**: `TurnRunnerDeps.correction = { maxPasses? }` (default 3,
   hard-capped at 5). Absent config is **byte-identical** to the single-shot
   behavior that already existed (goldens untouched) — this is additive
   machinery, not a replacement for the single-shot path.
2. **Every verification, including the first, is reported on the run's
   durable log** as a `run.verification` event (`{ pass, status,
   deficiencies }`, `pass` 0 = the run's own answer, then 1, 2, … per
   correction pass). Under single-shot (no `correction` configured), nothing
   changes — no events are emitted, so the absence of `run.verification` on
   a run's log is not evidence the run went unverified.
3. **Minimal re-context, not the full history.** A correction pass sends
   exactly three messages: the system prompt, one assistant message carrying
   the previous pass's visible answer, and one user-role deficiency
   write-back (a fixed template listing the host's lines verbatim,
   instructing the model to fix them with its tools). Replaying the whole
   conversation on every pass is what makes a correction harness
   unaffordable, and the model can re-read the domain through its own tools
   anyway — the more honest source, since the deficiencies were found *in*
   the domain, not in the transcript.
4. **Shrink-or-stall stopping rule.** Continue only when the new deficiency
   list is **strictly shorter** than the previous one, by count — not by
   set-difference. Deficiency lines are free-form host text, so a reworded
   line would read as progress under a set-difference comparison and loop
   forever; equal length, even with entirely different lines, is a stall.
5. **Fail-closed.** `verify()` throwing or returning `null` mid-harness is
   `status: "unavailable"` on the log and a full stop — never treated as a
   pass, and it never crashes the run (the fault goes to the `Logger`, not to
   an exception).
6. **The harness does not change the run's outcome.** A run with
   deficiencies surviving every pass still completes, exactly as the
   single-shot check does — failing a turn over a partial verification is a
   policy decision, and only the host that wrote the domain checks is
   entitled to make it.
7. **A correction pass is a full `runPass` on the same registry and the same
   event log**: tools staged exactly as the run had them, `seq` continuing
   unbroken, `UsageAuthorizer` asked before it and told after it, same as
   every other pass. There is no second code path, which is what makes "the
   harness cannot bypass spend control" true by construction rather than by
   a rule someone has to remember to apply.
8. **Records the harness writes are chain appends** (`activate: false`,
   chained off the run's own last write) — the same mechanism [ADR
   0007](0007-conversation-branching-fork.md) built, so a mid-run branch
   switch cannot migrate them onto a conversation that never ran them.
9. **The write-back is persisted, for audit, but never replayed.** It
   carries `metadata.correctionPass`, and `assembleMessages` skips every
   record carrying it — a write-back is an instruction aimed at one pass of
   one run ("fix these three items now"), and replaying it on a *later* turn
   would hand the model a dangling order about deficiencies that are already
   gone.
10. **Fixed the next day: `orderMessagesForProvider` now groups by
    tool-call linkage, not by record kind.** Each internal assistant turn is
    followed immediately by exactly the results answering its own declared
    `tool_call_id`s — a result is claimed once, by the first assistant turn
    that declared its id, removed from the unclaimed pool so a malformed run
    cannot attach the same result to two turns — and the groups replay in
    the order their assistant turns were written. This makes a run with two,
    three, or `maxPasses` tool-calling turns provider-legal by construction,
    where the old per-kind bucketing was only ever correct for a run with
    exactly one.

## Alternatives considered

- **Full conversation replay on every correction pass.** Rejected: cost
  grows with every attempt (the whole tool-call history, every pass), which
  is exactly what an affordable bounded correction needs to avoid; the model
  can re-read domain state through its own tools instead, which is also more
  honest since a stale transcript is not where the deficiency was found.
- **Set-difference, rather than count, for the shrink check.** Rejected:
  deficiency lines are free host text; a reworded but otherwise identical
  line reads as "a different set" under set-difference and would look like
  progress forever, defeating the stopping rule's purpose.
- **Keep bucketing tool-calling records by kind** (all internal assistants,
  then all tool results, then the visible answer), rather than by tool-call
  linkage. Rejected after the CRITICAL finding: correct only for a run with
  exactly one tool-calling pass, which the correction harness's entire
  purpose is to produce more than one of.
- **Fail the turn outright when a correction pass exhausts its budget still
  showing deficiencies.** Rejected: the harness reports, it does not judge —
  whether a partial verification should fail a turn is a domain policy
  decision belonging to the host that wrote the checks, not to the framework
  running them.

## Consequences

- [`docs/non-goals.md`](../non-goals.md)'s "Multi-pass correction harness"
  entry is removed — the gap it named is closed.
- Any host with a `VerificationHook` gets bounded self-correction for free by
  setting `TurnRunnerDeps.correction`; a host with none pays nothing
  (unchanged behavior).
- `orderMessagesForProvider`'s tool-call-linkage grouping is now the only
  grouping strategy in the codebase — it subsumes the old per-kind bucketing
  for the single-tool-calling-pass case (still correct, since one group is
  just one bucket) while additionally handling the multi-pass case the
  harness introduced.
- `run.verification` is the 13th event type in the vocabulary; it lands
  *after* the pass it describes on the durable log (pass 0 sits after that
  pass's `run.completed`) — an SSE consumer that closed on the first
  terminal event sees later correction passes only on a replay/poll of the
  log, never on the stream it originally opened, since the log's `seq`
  sequence stays unbroken.
- [`docs/roadmap.md`](../roadmap.md)'s "Multi-pass verification harness"
  Later entry moves to Done, referencing this ADR.

## Out of scope (deliberate)

Letting the harness fail a turn's outcome on unresolved deficiencies (a host
decision, not this framework's); per-deficiency-line tracking (set-based)
instead of count-based shrink detection; a UI-facing progress indicator for
an in-flight correction pass.

## Addendum (hardening tranche 2, F-OWN-5): the request is re-contexted too

Decision 3 above sent **three** messages. It now sends **four**: the system
prompt, **the request the run is answering**, the previous pass's visible
answer, and the write-back — in that order, because that is the order the
exchange happened in and a history that puts an answer before its question
replays as a different conversation. `CorrectionConfig.includeUserRequest`
(default `true`) turns the addition off.

The gap it closes: the model was being asked to correct work without being
told what was asked for. "Add the decoupling capacitors" and "add the
decoupling capacitors to U3 only" produce the same previous answer and the
same deficiency list, and a model that cannot see which one it was asked can
only guess — while the whole point of the harness is to make the second
attempt better-informed than the first. One message is the cheapest possible
fix, and it is the message the entire turn exists to answer.

What it deliberately does **not** become: the full history. The request is
read off the assembled conversation (the last `role: "user"` message the
provider actually saw, so a regenerate — which has no user message of its own
— still finds the question it is re-answering) and is sent as TEXT via
`messageContentToText`. A multimodal request contributes its words and not its
images: these messages are built rather than assembled from stored records, so
an image `ref` here would reach the provider unresolved, and re-sending
megabytes of pictures every correction round is exactly the cost the minimal
re-context exists to avoid.

**Divergence from OpenPCB recorded**, per the plan's decision 3: the ported
semantics sent the three messages, and this framework sends four by default.
A host that wants the original shape sets `includeUserRequest: false`.
