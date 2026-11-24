# Endpoint probes

Manual checks that send requests straight to an OpenAI-compatible Chat Completions
endpoint and look at what comes back. They answer one question: is a model plus its serving
stack (quantised weights, chat template, tool-call parser, gateway) good enough to drive an
agent? No AgentKit code and no application code runs, so a failure here cannot be caused by
AgentKit or by an application built on it. It is the model or the serving stack.

These are not part of `bun test` and not part of CI. They send real requests and cost
whatever the endpoint costs.

## Setup

Environment variables:

| Variable | Meaning |
|---|---|
| `AGENTKIT_LIVE_BASE_URL` | Endpoint base URL, up to and including `/v1`. Falls back to `LITELLM_BASE_URL`. |
| `AGENTKIT_LIVE_API_KEY` | API key. Falls back to `LITELLM_API_KEY`. Never written to disk. |
| `AGENTKIT_LIVE_MODEL` | Default model id when `--model` is not given. |
| `EVAL_OUT` | Root folder for results. Default: `./eval-results` in the current directory. |

Every script takes `--model <id>` and `--out <dir>`. `--out` overrides the default result
folder, which is `<root>/<script name>/<model>`. The default root is ignored by git.

## Scripts

Run from the repository root with `bun scripts/live/endpoint-probe/<name>.ts`.

- `probe.ts` is the baseline. It covers single, parallel and nested tool calls, strict
  schemas, calls without arguments, every `tool_choice` mode, a follow-up after a tool
  result, reasoning replay, system-message placement, selection among 30 tools, long code
  and non-English text as arguments, output cut off inside a tool call, thinking switched
  off, JSON-schema output, and a long-context lookup (29k to 241k tokens) plus a tool call
  after 60k tokens. Each case runs streaming and non-streaming. Options: `--repeats N`
  (default 5), `--only id1,id2`. At the defaults it sends about 230 requests. Writes
  `trials.jsonl` (every response) and `summary.json`.
- `diag-arg-markup.ts` answers: is a tool call lost or altered when the argument text
  contains markup such as `<tool_call>`, `</parameter>`, `</function>` or `<think>`? Each
  text variant is sent with the parser on (`tool_choice=auto`, streaming and non-streaming)
  and once with it off (`tool_choice=none`), so a lost call can be traced to the parser
  instead of the model. Option: `--repeats N` (default 3). At the defaults it sends 63
  requests.
- `diag-markup-injection.ts` answers: does tool-call markup inside data (a tool result or
  the user's own text) come back as a real call? A call to the tool named inside the data
  is a phantom call. `--neutralise` rewrites the markup before sending, to test that
  mitigation. Option: `--repeats N` (default 10). At the defaults it sends 40 requests per
  mode (plain and neutralised).
- `replay.ts <request.json>` re-sends one recorded request (a file with the request body,
  or a wrapper with a `body` field) `--repeats` times (default 10), optionally with one
  change (`--set '{"temperature":0.7}'`, `--label name`). It shows whether a failure is
  stable and whether a setting the application controls moves its rate. It sends exactly
  `--repeats` requests.

## Template case set

A second set of cases for a Qwen3.x model behind vLLM (XML tool-call template, `qwen3_coder`
parser, thinking on by default). Select it with `AGENTKIT_LIVE_CASE_SET`: `base` (default, the
cases above), `template` (only the new set) or `all`. Results go to `<root>/probe-template/<model>`
(`probe-all/<model>` for `all`), so a base run in the same `EVAL_OUT` is not overwritten. The
runner reads `max_model_len` from `GET /models` once; context cases are skipped when it is unknown
or too small. Each detail line records what was observed, so the results read as findings.

- `effort-low`, `effort-medium`, `effort-xhigh`: `chat_template_kwargs.reasoning_effort` is accepted and the model still thinks; detail has reasoning tokens.
- `effort-minimal-rejected`: a value the template does not know gives HTTP 400.
- `effort-top-level-field`: is a top-level `reasoning_effort` honoured (compare its reasoning tokens with `effort-low`).
- `thinking-off-plain`: `enable_thinking=false` gives a plain answer with no thinking text.
- `replay-thinking-two-steps`: two assistant steps replayed with `reasoning_content`, then a text answer with no leaked markup.
- `replay-thinking-reasoning-field`: history thinking under the field name `reasoning` (what vLLM emits) is accepted.
- `arg-typed-values`: integer, number, boolean, array, object, enum and null keep their JSON types.
- `arg-numeric-looking-string`: `04001` and `true` in string fields stay strings.
- `arg-multiline-exact`: an 8-line argument with an empty line, leading spaces and a trailing backslash is exact.
- `arg-closing-tags-literal`: `</parameter>` and `</function>` on their own lines inside an argument do not cut it short.
- `user-pasted-tool-response-only`: a lone `<tool_response>` user message; shows the template's "no user query" hazard.
- `user-pasted-tool-response-later`: the same block as a later user turn is answered.
- `length-inside-thinking`: a cut-off inside thinking is reported as `finish_reason=length`.
- `tool-after-120k-context`: valid tool call after ~120k tokens (needs `max_model_len` of at least 140000).
- `over-context-rejected`: a prompt over `max_model_len` gets 400 naming the limit.
- `image-input`: a 64x64 red PNG data URL is accepted and named.

## Reading a failure

Probe output lists, per case and mode, passes out of trials and the first failure detail.
Full responses are in `trials.jsonl`.

- A failure that repeats the same way is a property of the model or serving stack. Nothing
  in AgentKit or an application can have caused it.
- The serving stack is the likely cause when the response contradicts itself, for example
  `finish_reason` says a call is complete while the output hit the token limit, or the same
  generation differs between streaming and non-streaming.
- Do not rank a weak spot from one failed trial. Raise `--repeats` or use `replay.ts`.
- Whether an application can work around a failure is a separate question: replay with one
  change the application controls and compare rates.

## Sibling check

`rust/crates/providers/src/bin/endpoint_probe.rs` is the existing Rust check of the
provider layer against an endpoint. These scripts differ in having more cases: output cut
off inside a tool call, tool-call markup inside argument text and inside data,
system-message placement, and long-context lookup.
