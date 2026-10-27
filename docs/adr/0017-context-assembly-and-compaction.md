# ADR 0017 — Context assembly and compaction: transcript replay, token budget, project instructions

**Status:** part 1 accepted, implemented (2026-10-27; milestone M3a of [ADR 0015](0015-unified-agent-runtime.md)).
Part 2 (compaction) is a placeholder until M3b.
**Contract impact:** additive only. Four new `run.warning` codes (`context_budget_fallback`, `context_trimmed`,
`skill_unavailable`, `project_instructions_unavailable`), a new host `errorCode` (`context_overflow`), optional
session-descriptor fields (still `version: 1`), and `AiProviderModel.contextWindowTokens` now populated by
`OpenAiCompatibleClient.listModels`. `CONTRACT_VERSION` and sqlite `SCHEMA_VERSION` do not change.

## Problem

[ADR 0016](0016-durable-agent-sessions.md) gave a coding agent a durable session, but each turn still starts from a
thin context. A real task spans many turns of reading files, editing and running tests, and the model needs all of it:
- earlier turns' tool calls, results and steering, not only their final answers;
- a budget derived from the model's context window, because a transcript grows without bound and an over-limit
  request fails at the provider with an error that names neither the cause nor the way out;
- the project's own rules (`AGENTS.md`, `CLAUDE.md`) and the Skills the session selected.

Non-session chats must keep assembling byte-for-byte as they do today.

## Evidence

- `assembleMessages` (`packages/host/src/turn/history-assembly.ts`) replays what `anchoredHistory` returns: the
  placeholder's ancestors plus its own run's records. Run records are appended with `activate: false`, but a chain
  append inherits `active` from a parent that is active and still the end of the live chain (the
  `AppendMessageInput.activate` contract). So whether an earlier run's records are on the active path depends on how
  the next turn was submitted: a plain `send` follows the active leaf (records replay), while a follow-up converted at
  settlement hangs under the placeholder (records are off the path and only the final answer replays). This corrects
  ADR 0016's "later turns see final answers only", which holds for the second shape only. Transcript replay must
  therefore reach the run's own records explicitly and not duplicate the ones already on the path.
- `OpenAiCompatibleClient.listModels` dropped everything but `id` and `name`, although `AiProviderModel` already has
  `contextWindowTokens`, and `AiProviderCapabilities.maxContextTokens` was only ever set by hosts by hand. Nothing in
  the host persisted a probed catalogue (`ProviderOperations.refreshModels` is a transport port with no host
  implementation).
- The estimate `UsageAuthorizer` receives (chars / 4, images free) is too crude to budget against: a screenshot costs
  ~1200 tokens.
- Host has `types: []`, so reading `AGENTS.md` needs a port; `@agentkit/runner-local/filesystem` already carries the
  confined-read pattern for Skills (`realpath`, `O_NOFOLLOW`, identity recheck, size cap).

## Decision (part 1)

### 1. Context capacity

- `OpenAiCompatibleClient.listModels` maps the first finite positive integer of `max_model_len` (vLLM),
  `context_length`, `context_window`, `max_context_length` to `contextWindowTokens`. Nothing else changes.
- `resolveContextWindow` (`packages/host/src/context/budget.ts`) picks, in order: descriptor `context.windowTokens`;
  the stored model's `contextWindowTokens`; the provider capability `maxContextTokens`; else 32768 with
  `source: "fallback"`. A fallback emits one `context_budget_fallback` warning per session turn.
- `refreshProviderModels(deps, providerId)` (host) calls the client's `listModels` and `replaceModels`. It is explicit,
  never per turn: it costs a request to someone else's server. A host wires it into `ProviderOperations.refreshModels`.

### 2. `TokenEstimator` (core)

`estimateMessage` / `estimateText` and `estimateTools`, default ceil(chars / 4): an image part counts 4800 chars, an
assistant's tool calls count name plus `argumentsJson`, audio counts its data, tool definitions count their JSON.
The constants follow Pi's `estimateTokens` (`packages/coding-agent/src/core/compaction/compaction.ts`, MIT, Mario
Zechner, upstream commit 4a080252f06caf63a525d7873c5d94d3c23887e0); no code is copied. `TurnRunnerDeps.tokenEstimator`
swaps it. `AiChatMessage` carries no reasoning text, so there is none to count.

### 3. History policy

`assembleMessages(deps, chatId, anchor, systemPrompt, { policy })`. `final_answers` (default) is today's behaviour.
`transcript` additionally takes, for every prior visible assistant placeholder on the path, that run's own records
(`lastMessageOfRun` back to the placeholder), minus any already on the path, and lets `orderMessagesForProvider` put
them before the placeholder's final content (the placeholder carries the run's lowest order key). Internal assistant
tool-call records, tool results and steering user records replay in order. Every existing filter
(`canonicalProviderDisplay`, correction write-backs, orphan tool results, `reconcileOrphanToolCalls`) still applies. A
prior run whose records no longer descend from its placeholder replays as its final answer alone: one odd old turn
must not fail every later turn. Session turns use `transcript` unless the descriptor says `context.policy:
"final_answers"`.

### 4. Budget

`fitToBudget` runs for session turns only, after assembly and before run-content messages are added:
- reserve = descriptor `context.reserveTokens` ?? 16384, clamped to at most half the window;
- budget = window - reserve - tool definitions - run-content messages;
- a turn is a user message plus everything up to the next turn start; a user message directly after a tool result is
  a steer and belongs to its turn;
- over budget, drop the oldest whole turns after the system message. A call and its results always live in one turn,
  so a pair is never split. The system message and the current turn are never dropped;
- `droppedTurns > 0` emits `run.warning` `context_trimmed`. If it still does not fit, the turn fails with
  `errorCode: "context_overflow"` and the provider is never called (same landing as `no_model`).

### 5. Project instructions

Port `ProjectInstructionsSource.read(root, name, maxBytes)` (`TurnRunnerDeps.projectInstructions`), filesystem
implementation `fileProjectInstructions()` in `@agentkit/runner-local/filesystem`: `realpath` on the root, a plain
file name only, `O_NOFOLLOW`, lstat/fstat identity, symlinks and non-regular files refused. `AGENTS.md` then
`CLAUDE.md`, 64 KiB each, truncated with a warning. Each becomes a system block `Project instructions (<file>,
sha256:<12 hex>)` appended to the system prompt of sessions whose descriptor has `workspace: { root, trusted }`.
`trusted: false` puts a line before the block saying it is untrusted repository text and never a permission grant. The
text never grants a tool anything: guards decide what runs.

### 6. Session Skills

Descriptor `skills: { id, revision }[]` becomes `contentSelection.skills` on every session submit and every
converted follow-up. A Skill that fails to activate (missing, revision or hash changed, no provider wired) is
reported through `RunContentContext.onSkillUnavailable`, which makes the run-content provider skip it instead of
throwing. The turn emits `skill_unavailable` naming the id and continues. Without the callback (non-session turns) a
failing selection still fails the turn as before.

### 7. Compatibility

Non-session chats take the same code path as before: `final_answers`, no budget, no instructions, no warnings. Golden
traces are byte-identical. The descriptor validator now rejects a present-but-malformed `workspace`, `context` or
`skills` (it used to ignore unknown keys; these are no longer unknown).

## Alternatives considered

- **Budget every chat.** Rejected: "non-session chats are byte-identical" is a release constraint, and a plain chat's
  history is already bounded by `historyLimit`.
- **A real tokenizer.** Rejected for now: one per provider family, kept in step with releases, to improve a number the
  budget only uses to answer "roughly how full". The estimator is a port for hosts that have one.
- **Trim by single messages.** Rejected: dropping half a tool-call group makes providers reject the request.
- **Read `AGENTS.md` through the existing tool pack.** Rejected: instructions must be in the system prompt before the
  first model call, and a model-driven read can be skipped.
- **Auto-refresh the model list on every turn.** Rejected: a network request on the hot path for data that changes
  rarely.

## Consequences

- Session transcripts grow with real tool output, so a small-window model trims often; `context_trimmed` makes that
  visible, and part 2 replaces the oldest turns with a summary instead of losing them.
- The 32768 fallback is a guess. Hosts running local models should refresh models or set `context.windowTokens`.
- Instruction files are repository text. They are labelled with their hash and trust, but a trusted workspace is the
  caller's assertion, not something this layer verifies.

## Part 2: compaction (M3b) — placeholder

To be written with M3b: summary records, the cut point, branch and fork invalidation, and the `run.context.compacted`
event. Until then, trimming above is the only reduction and drops information.

## Verification

- `packages/host/tests/context-transcript.test.ts`: replay order across three turns with tools and a steer (off-path
  and on-path shapes), non-session history unchanged, trimming at turn granularity on a 4000-token window with no split
  pair, overflow refusal with no provider call, fallback warning, descriptor override, instruction injection with the
  trust prefix and hash label, Skills carried over and `skill_unavailable`.
- `packages/core/tests` (`listModels` parsing, estimator) and `packages/runner-local/tests/project-instructions.test.ts`
  (read, fallback, truncation, symlink and escape refusals, symlinked root, missing file).
- The gate, byte-identical golden traces, `scripts/tests/snapshot-release.test.ts`, the architecture test.
