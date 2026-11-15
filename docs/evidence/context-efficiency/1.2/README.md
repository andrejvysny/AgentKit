# Package 1.2: turn context

Qualification: tests written, NOT run. Validation is deferred to the owner's
milestone gate. Nothing in this note is a claim that a gate passed.

Requirements: CE-01 (selected content inside the user message), CE-03 (turn
context persisted and replayed unchanged), INV-2, finding F1. ADR:
`docs/adr/0036-turn-context.md`.

## Built

- `AiTurnContextPart` contract: Rust DTO, schema bundle (the new schema and
  every inlined `AiContentPart` union), TypeBox, fixtures (schema count 134 to
  135), hand-edited API snapshots.
- `agentkit_core::layout`: `turn_context_part`, `content_for_wire`,
  `escape_turn_context`, `fold_selected_content`.
- `agentkit_chat::turn_context`: `attach` (build once, persist with the user
  message and the session record in one transaction), `render` (format and
  4,000-token budget), `state` (record, `append_turn_note`).
- The chat executor no longer fills `ChatRequest::selected_content`; Chat
  Completions and Responses fold any caller-provided selection into the last
  user message and no longer put it ahead of or into the system segments.

## Tests written (not run)

| File | Covers |
| --- | --- |
| `rust/crates/core/tests/layout.rs` | wire conversion, fold, tag escaping |
| `rust/crates/contracts/tests/parity.rs`, `tests/support/dto_roundtrips.rs` | schema, typed round trip |
| `rust/crates/providers/tests/turn_context_layout.rs` + `tests/fixtures/turn_context/*.json` | CE-01 golden wire bodies for Chat Completions and Responses |
| `rust/crates/providers/src/responses_request.rs` (unit) | selection inside the user item, continuation |
| `rust/crates/chat/src/turn_context/{render,attach}.rs` (unit) | format, budget, placeholder and handle, planning, deselect note, date, civil date |
| `rust/crates/chat/tests/turn_context_persist.rs` | one skill entry, CE-03 replay after a restart, queued notes, date, oversize placeholder |

Updated to the new layout: `chat/tests/native_parity.rs`, `http/tests/http.rs`,
`packages/native/tests/native-admission.test.ts`.

## Not covered or deferred

- Anthropic: the request builder is in OneMind; it must read the user message
  parts and stop reading `selected_content`. The golden Anthropic fixture
  belongs to that package.
- `rust/api/agentkit-contracts.txt` and `docs/rust/api/*.md` were edited by
  hand; regenerate with the gates.
- Compaction, masking and per-branch records (ADR 0036 consequences).
