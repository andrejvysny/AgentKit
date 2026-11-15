# Package T2.2: OpenAI-style lane

Requirements: TU-16, TU-17, TU-18, TU-21 (and the TU-02 fix-up). Tests
written, NOT run (validation deferred to the owner).

## What was built

- `core/model_profile/request.rs`: `resolve_request_settings` (sampling set by
  thinking on/off, effort channel, `chat_template_kwargs`, `parallel_tool_calls`,
  tool-choice policy with a recorded downgrade), `ToolChoice`,
  `QWEN_EFFORT_TEMPLATE_KEY` (unverified, probe P6), `REASONING_HISTORY_FIELD`.
- `core/model_profile/messages.rs`: `apply_message_rules` (one system message
  first, later system text into user turns, at least one user message, escaped
  tool-response tags, images only where allowed).
- `core/model_profile/salvage.rs`: Qwen XML call parser.
- `ChatRequest` gained `profile`, `reasoning_off`, `tool_choice`; `chat.rs` sets
  `profile` from the session's frozen record.
- `providers/chat_completions_request.rs` (moved `build_body`) applies all of
  it; `chat_completions.rs` salvages and emits warnings.
- `run.rs`: a step's reasoning rides on its assistant message; a salvaged turn
  marks `salvaged: true` on its `run.tool.record`.
- `chat/history.rs`: stored `reasoningContent` rides on replayed assistant
  messages, once per run.
- Fix-ups: malformed history arguments become `{}` with a
  `history_arguments_sanitized` warning (always on); the empty-answer re-ask
  emits `tool_less_retry` like the chat-only retry.

## Tests written (not run)

- `core/tests/profile_request.rs`: both sampling sets, effort word via template
  key and never the OpenAI field, unmapped level, auto-only downgrade, forcing
  profiles, body-extras merge, message rules, salvage parser.
- `core/tests/profile_run.rs`: reasoning on the next request's assistant
  message; salvaged flag on the record.
- `providers/tests/openai_lane.rs`: Qwen and default request snapshots, thinking
  off, two-turn history with and without send-back, history sanitiser,
  tool-choice downgrade warning, salvage on/off.
- `chat/tests/chat_correction.rs`: the empty-answer re-ask warning.
- `chat/src/chat/history.rs` unit test: reasoning dedupe across a run.

## Not done / unverified

- `QWEN_EFFORT_TEMPLATE_KEY = "reasoning_effort"` is a guess until probe P6.
- Hosts do not yet set `reasoning_off` or `tool_choice`; nothing requests them.
- Responses lane: unchanged (OpenAI-native fields already match the hosted
  profiles); Qwen is Chat Completions only.
- Nudge stays off: the profile flag exists, no hook was added.
- Context budgeting does not count sent-back reasoning tokens.

## Follow-up: warning codes, Responses lane parity, reasoning tokens

- All profile/pipeline warning codes are in `AiRunWarningCode` (`packages/contracts/src/run-events.ts`) and the table in `docs/contracts.md`.
- Responses lane: `build_responses_request_notes` applies the history-arguments sanitiser (shared `history_arguments`) and the profile tool-choice policy (`policy_tool_choice`, shared with Chat Completions); notes become `run.warning` events. Tests `providers/tests/openai_lane.rs::responses_*` (written, NOT run).
- `chat/context_budget.rs::message_tokens` counts the `reasoningContent` metadata (unit test `sent_back_reasoning_counts_toward_message_tokens`, written, NOT run).
