# ADR 0037: Delegation contract

Status: accepted (2026-11-15). Implements DEL-01 to DEL-04 of the delegation
spec (roadmap package 4.1). Builds on ADR 0003 (subagents), ADR 0034 (output
store) and ADR 0022 (Rust owns the wire contract).

## Context

`subagent_spawn` took one free-text `prompt`. The child saw whatever the
parent chose to paste, its result came back as unbounded text, and nothing in
code said that the text was a report and not an instruction. Package 4.3 wrapped
the child text of a parked wait in an untrusted block; the brief, the envelope
and the other trust rules were open.

## Decision

1. **A structured brief replaces `prompt`** (clean break, no shim). Required:
   `role`, `title` (at most 80 characters), `objective`, `output_format`
   (`summary`, `findings`, `review`, `patch_report`) and `scope.stop_when`.
   Optional: `scope.include`, `scope.exclude`, `context_refs`, `budget`,
   `model`, and the existing `exclusive_workspace`. A call that misses a
   required field is refused with a message that names it
   (`invalid_brief: missing required field "scope.stop_when"`). The tool schema
   lives with the tool (it is not in the contracts bundle), so no bundle or
   fixture changes. Two names differ from the spec because the schema lint
   forbids them: the budget timeout is `budget.timeout` (milliseconds, like
   `subagent_await`), and `spawn_key` stays the runtime-derived idempotency key
   the model never sends (TU-06).
2. **One fixed template per role renders the child's first message:** role
   prompt, objective, scope, expected output (fixed text per format),
   references, budget. Every heading and instruction is constant; only the
   brief's values vary, so siblings of a role share the prefix up to their own
   objective. A golden file pins the text. Quoted message text is prefixed on
   every line so it cannot forge a heading. The budget section shows the limits
   the child actually runs under, read back from its durable metadata, so a
   replayed spawn renders the same message.
3. **Context by reference.** `output` refs are output handles: the parent must
   be able to read the handle itself (own session, a descendant's, or a handle
   it was granted), else the spawn is refused with the same text for a missing
   and an unreadable handle. The grant is a record on the child's chat metadata
   (`agentkit.subagent.outputGrants`, keyed by the child session). `SessionTree`
   gains `may_read_handle(caller, owner, handle)`, whose default is the tree
   rule of `may_read`; the SQLite adapter and the in-memory twin also accept a
   handle the caller's record lists. `read_output` and `search_output` call it,
   so a child reads exactly the listed handles and nothing else of its parent's
   or siblings' outputs. `path` refs are confined to the inherited workspace
   (`..` is resolved first, and an existing path is checked again after links
   resolve). `memory` refs are validated as record ids and passed through
   (memory tools arrive with 3.3). A `message` ref copies one user or assistant
   message of the parent, cut at 4 KiB.
4. **The result envelope** is exactly the spec's: `child_id`, `role`, `status`
   (`completed`, `failed`, `cancelled`, `budget_exhausted`, `timeout`),
   `summary`, `findings`, `artifacts`, `open_questions`, `usage`, `truncated`,
   `full_result_handle`. The service wraps the child's final message; it does
   not ask for JSON. `summary` is that message cut at 6 KiB on a paragraph
   boundary. A longer message is written whole to the output store (under the
   child's session, so the parent reads it as a descendant's output), the handle
   is remembered on the child's chat so repeated reads name the same handle,
   and `full_result_handle` and one `output` artifact carry it. `findings` and
   `open_questions` come only from the child's own `report_findings` call, a
   tool only a child session is offered (the contributor adds it for subagent
   sessions; the role guard hides and refuses it elsewhere). `usage` sums the
   settled `run.usage` events of the child's turns (normalized counters, with
   the prompt and completion totals as a fallback) and the turns' wall clock.
5. **Presentation.** Every tool that shows a child to the parent (`subagent_status`,
   `subagent_list`, `subagent_cancel`, `subagent_resume`, `subagent_await`, and
   the parked-wait answer) carries the envelope JSON as the text of
   `<subagent_result child= role= trust="untrusted">`; the raw `result` and
   `error` fields are removed. The closing tag is neutralized inside the text.
6. **Trust rules in code.**
   - `SubagentService::answer_as(actor, input)` refuses with
     `child_cannot_answer` when the actor session is a subagent, whoever raised
     the request. Any agent-originated answer goes through it. Child waits stay
     the waker's.
   - A child's approval request stays in the child's session and reaches the
     owner through `pending_owner_requests(root)`, which lists every pending
     human request of the tree with a label `<child title> (<role>)`.
   - The tool record of a resolved child wait is stamped `provenance:
     "child_agent"` and `ownerText: false`. `is_child_derived_message(metadata)`
     is the one test a memory extractor (package 3.4) uses to exclude it; it
     also matches the result records of any `subagent_*` tool, whose ordinary
     tool-result message carries no stamp.
   - A child result is only ever a tool record. The delegation guidance is the
     only subagent text in a system segment and never carries a result.
7. **Delegation guidance** for segment S2 is the constant
   `DELEGATION_GUIDANCE`: when to delegate (broad or parallel read-only work),
   when not to (sequential reasoning, small tasks, anything that needs the
   conversation's nuance), and the effort (one child for a focused lookup, up
   to four for a broad survey). It is the default of `SubagentConfig`.

## Consequences

- A spawn call is longer; the model must write a brief. The guide and the tool
  examples show it.
- `max_tokens` is accepted, clamped to 1,000,000 and recorded on the child, but
  not enforced until the token budgets (DEL-06) land.
- The public API of `agentkit-core`, `agentkit-store` and `agentkit-agents`
  changes: `SessionTree::may_read_handle`, the grant keys, `Brief`,
  `ResultEnvelope`, `SpawnRequest { brief }`, `AwaitResult { envelope }`,
  `answer_as`, `pending_owner_requests`, `present_child_results`,
  `is_child_derived_message`.
- A child that fails or is cancelled has no final message; its `summary` is the
  error text (still inside the untrusted block).
