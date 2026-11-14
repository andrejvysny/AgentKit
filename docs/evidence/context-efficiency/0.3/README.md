# Package 0.3: prefix-stability harness

Base: package 0.2, `2506b6ef1e295daef398110ad65fb9305181b3d0` on master.
Requirement: CE-02 baseline infrastructure. CE-02 enforcement is not complete.

## Scope and decisions

The owner approved staged transport coverage: AgentKit Chat Completions and
Responses now; OneMind Anthropic after the 0.6 vendor refresh; all three
transports must be enforced in 1.8. This removes the dependency cycle without
editing OneMind before its gate.

The runtime example uses the real ChatExecutor, AgentHost, session service,
SQLite repository and provider request assemblers. Scripted trusted transport,
read tool and contributor fixtures live in testkit, following ADR 0021 R10.
The transport depends on the engine port, not the concrete SQLite adapter.
Only example/dev infrastructure changes; production assembly remains unchanged.

Both profiles are custom AgentKit fixtures, not OneMind defaults. Their session
context explicitly enables existing tool-result elision. Each scenario has
30 turns. Control sessions exercise tools; mutation sessions change project
instructions before turn 5, managed selection before turn 7 and contributor
visibility before turn 9. Separate Responses selection/tool scenarios continue
beyond the combined scenario's early failure.

Comparison uses the literal UTF-8 serialization of each assembled request Value
through its previous last input item, preserving outer and nested field order
and raw argument strings. Offsets are zero-based bytes in that serialization.
These are not observed HTTP wire bytes or provider token streams. Fields after
the input array, including Responses instructions/tools, are outside this
literal prefix; their presence in retained captures must not be mistaken for
proof that their changes preserve a provider's prompt cache. Live semantics
and all transport layout enforcement remain future gates.

Only durable context.cache_break events between dispatch marks exempt a pair.
Complete event logs are paginated; fractional event cursors are preserved.
Source hypotheses are explicitly provisional. The source checks below and raw
capture differences substantiate this baseline; ordinal hints are not proofs.

## Baseline findings

`baseline.json` lists every measured break, request pair, byte offset, path,
status and failure event. `captures.json.gz` retains assembled Values, their
serialized bodies and complete durable events for independent inspection.
No cache-break events occurred in these scenarios.

- Chat Completions controls complete all 30 turns/60 requests. Both profiles
  rewrite an old result between requests 4 and 5, at byte 2390,
  `/messages/3/content`. The original large tool result becomes an elision
  stub. Source: chat/src/chat.rs:352 and chat/src/chat/elision.rs:159.
- Chat Completions combined scenarios complete all turns and each have three
  prefix breaks: elision, project instruction replacement (requests 8/9),
  and managed selection (requests 12/13). Exact offsets/paths are in JSON.
  Project augmentation is repeated at chat/src/chat.rs:278; managed content
  is resolved at :182 and inserted during assembly. Contributor changes
  are restaged in chat/src/chat/hooks.rs:433-458 but lie after the measured
  input prefix in these serializers.
- Responses controls complete all 30 turns/60 requests without a measured
  literal prefix break. Its continuation lane skips sliding elision.
- Responses combined scenarios dispatch eight requests, then fail all
  remaining 26 turns with continuation_history_mismatch. The events do not
  identify which of responses_request.rs:68,169 rejected the state. Both
  guards are retained; no guard is weakened to make the baseline green.
- Responses selection scenarios complete all turns but rewrite canonical
  history between requests 12/13, at byte 111, `/input/1/@order`. Captures
  show the old provider function_call replaced by an assistant message
  when selection changes and continuation is rebuilt. `@order` denotes
  an object-key/layout difference, not an actual JSON property.
- Responses tool-only scenarios complete all turns without a measured
  literal prefix break. This does not establish semantic cache stability
  for fields serialized after input.

## Reproduction and validation

From the repository root, with local dependencies already available:

```sh
cargo +1.97.0 test --manifest-path rust/Cargo.toml --locked -p agentkit-testkit --test prefix_comparison --test context_events
cargo +1.97.0 test --manifest-path rust/Cargo.toml --locked -p agentkit-runtime --example context_capture
cargo +1.97.0 run --manifest-path rust/Cargo.toml --locked -p agentkit-runtime --example context_capture -- --baseline
cargo +1.97.0 run --manifest-path rust/Cargo.toml --locked -p agentkit-runtime --example context_capture -- --baseline-captures
node scripts/gates.mjs
```

The capture command writes JSON to stdout only. The committed gzip uses mtime=0;
the decompressed bytes have SHA-256
`b1042fbfb5c80e720df72b7f2688b37ee63029345710711a3dc6d1a1bedab170`.
Temporary fixture workspaces and generated event identities can vary between
runs; compare semantic findings, not hashes of independent runs.

Targeted checks: seven comparator tests, one 1001-event pagination test and
four runtime example tests passed. Review found order normalization, fractional
cursor truncation and bounded event reads; parent fixes are covered by the
attached substantive failing tests and passing reruns. Existing prototype
comparator tests also failed before its implementation; no integration red is
claimed for initially unregistered tests. Rust API snapshot reflects the
intentional testkit additions. Architecture allowlist unchanged.

Full local gate results are recorded below after qualification. Anthropic,
paid provider cache reads and production task-success claims are not verified.

## Final qualification

All 18 local gate checks passed in the isolated source copy: 2,157 Rust tests
across 243 binaries, 246 Client/React tests and 753 native/core tests; format,
clippy, dependency, architecture, Rust API, contracts, typecheck, lint, fixture,
native/TypeScript/umbrella builds, public API mirrors and Node smoke all passed.
`gates.log` retains the runner summary. Native process ownership checks ran
outside the outer sandbox so their process inspection was available. All package
source files were compared byte-for-byte against master before staging.
