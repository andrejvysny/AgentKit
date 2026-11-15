# Package 1.1: layout contract and session prefix

Qualification: tests written, NOT run. Validation is deferred to the owner's
separate pass. Nothing in this note is a claim that a gate passed.

Base: AgentKit `master` at `7e55c91`. Requirement: CE-01 (system segments and
the session prefix record). ADR: `docs/adr/0028-cache-stable-prompt-layout.md`.

## Built

- `agentkit_core::layout`: `Segment` (S1-S6), `PromptLayout` (system blocks
  S2-S4), `PrefixDigests`, `freeze`, `cache_key`.
- `AiSessionPrefix` contract: Rust DTO, schema bundle entry, TypeBox schema,
  regenerated fixtures (schema count 131 to 132). Optional tool-track fields:
  `toolManifestHash`, `modelProfileId`, `dialectVersion`.
- Persistence under chat metadata `agentkit.session.prefix`
  (`agentkit-sessions` helpers); recorded and compared in `chat/prefix.rs`.
- Chat executor builds S2 (profile, context, delegation) and S3 (project rules)
  as separate tagged system messages. Compaction and trimming now skip all
  leading system messages instead of one.
- Chat Completions sends them as separate `system` messages. Responses keeps S2
  in `instructions` and sends S3/S4 as leading system-role input items that are
  stripped from the continuation checkpoint.
- Harness (still report mode): `layout_violations`, `system_block_count`, a
  `layout` section per scenario, and `prefix_refrozen` counted as a recorded
  cache break.

## Tests written (not run)

| File | Covers |
| --- | --- |
| `rust/crates/core/tests/layout.rs` | segment order, block omission, tagging, freeze rule, cache key |
| `rust/crates/contracts/tests/session_prefix.rs` | schema, round trip, refusals |
| `rust/crates/providers/tests/prompt_layout.rs` | CE-01 golden bodies for Chat Completions and Responses, checkpoint stripping |
| `rust/crates/chat/tests/session_prefix.rs` | record once, keep, re-freeze with durable warning, tagged order |
| `rust/crates/testkit/tests/prefix_comparison.rs` | wire layout check |
| `rust/crates/runtime/examples/context_eval/tests.rs` | report carries layout rows |

## Not covered or deferred

- Anthropic: the request builder is in OneMind (vendored AgentKit). AgentKit
  exposes tagged messages and `PromptLayout::text_blocks`; the golden Anthropic
  fixture belongs to the OneMind package that consumes them.
- Selected content still precedes S2 (package 1.2).
- The committed 0.3 baseline (`baseline.json`, `captures.json.gz`) is not
  regenerated; it needs a run of the capture example.
- Public API snapshots (`rust/api/*.txt`) and TypeScript API reports were not
  regenerated.
