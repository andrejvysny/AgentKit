# Package 0.4: replay harness and baseline bill

Base: package 0.3, `38eacd71620a373cfe883ea056ccc76bd10f92e1` on master. Requirement: CE-17 evaluation infrastructure only.
CE-17 acceptance is not complete.

## Scope and decisions

The frozen corpus contains five Chat and five Code sessions. Each session has
three turns and six actual provider-assembled requests from the real host,
session service, SQLite store and request serializers. Sessions are independent;
no prior session warms another's cache. Inputs, provider frames and tool results
are synthetic. The outcome label is fixed to `script completed` and validated
against actual terminal task statuses. This is not evidence of production task
success, provider pricing or savings.

The replay command reads each frozen session through current runtime assembly
and compares ordered serialization with the recorded request. Raw argument
strings are preserved. Each recorded frame's output is checked against durable
message/tool events. Unsupported multipart or mixed outputs fail closed. Outcome
labels and handle counts cannot be relabelled as observed task success or handle
measurements: this driver has no handle instrumentation, so counts remain null.
It never records over its input. The scorer uses a fixed repository verifier,
not a caller-supplied executable, rechecks input bytes and exclusively creates
its output file.

No paid provider or model downloads were used. Choosing synthetic sessions is
necessary for a reproducible, offline foundation without authorized paid runs.
A later candidate must replay a comparable corpus and pass actual task-success
checks before CE-17 can close.

## Baseline

`baseline.json` is the deterministic report for `sessions.json`. Cache lifetime
is 300,000 ms and minimum eligible size is 256 estimated tokens. Token estimate
is UTF-8 serialization bytes divided by four, rounded down. Cache reads use the
longest common prefix among unexpired requests per key. Writes are zero under
this explicitly automatic mock-cache policy, not an Anthropic write-price claim.

Rates are versioned `illustrative-v1`, in abstract units per million tokens:
uncached input 2; read 0.2; short write 2.5; long write 4; output 8; reasoning 8.
They are illustrative inputs, not current provider prices. Reasoning is unknown
in every session, so all finite amounts below are lower bounds and upper bounds
are null. Output and reasoning are disjoint counters.

| Session | Profile | Requests | Uncached | Cache read | Output | Bill lower bound | Tools | System | History | Turn context |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| chat-0 | chat | 6 | 1140 | 607 | 15 | 0.0025214 | 150 | 192 | 1065 | 114 |
| chat-1 | chat | 6 | 1586 | 703 | 15 | 0.0034326 | 306 | 150 | 1355 | 186 |
| chat-2 | chat | 6 | 1317 | 749 | 15 | 0.0029038 | 150 | 192 | 1388 | 114 |
| chat-3 | chat | 6 | 1765 | 845 | 15 | 0.0038190 | 306 | 150 | 1671 | 186 |
| chat-4 | chat | 6 | 1242 | 1149 | 15 | 0.0028338 | 150 | 192 | 1704 | 114 |
| coding-0 | code | 6 | 1144 | 609 | 15 | 0.0025298 | 150 | 192 | 1065 | 114 |
| coding-1 | code | 6 | 1590 | 705 | 15 | 0.0034410 | 306 | 150 | 1355 | 186 |
| coding-2 | code | 6 | 1323 | 751 | 15 | 0.0029162 | 150 | 192 | 1388 | 114 |
| coding-3 | code | 6 | 1768 | 848 | 15 | 0.0038256 | 306 | 150 | 1671 | 186 |
| coding-4 | code | 6 | 1242 | 1153 | 15 | 0.0028346 | 150 | 192 | 1704 | 114 |

Component estimates exclude JSON separators and request parameters; they are
not additive to billed estimates. System includes combined profile/project text
where the current serializer cannot attribute it separately. Project, memory
and follow-the-handle counts remain null. No silent zero or invented attribution
is substituted. Full per-request classes, frozen prices and unknown fields are
retained in the report.

## Reproduction and validation

```sh
cargo +1.97.0 test --manifest-path rust/Cargo.toml --locked -p agentkit-runtime --example context_capture
bun test scripts/evals/context-replay
bun scripts/evals/context-replay/score.ts docs/evidence/context-efficiency/0.4/sessions.json /tmp/new-context-bill.json
node_modules/.bin/biome format --stdin-file-path=docs/evidence/context-efficiency/0.4/baseline.json < /tmp/new-context-bill.json > /tmp/new-context-bill.formatted.json
cmp docs/evidence/context-efficiency/0.4/baseline.json /tmp/new-context-bill.formatted.json
node scripts/gates.mjs
```

The output path must not already exist. The committed corpus SHA-256 is
`f4d831988644b3f48d78055db6f5466ec13033003567ba88a46c9bb31e9d81ba`.
The frozen-report equality test guards deterministic configuration and prices.
Parent review verified the actual diff and ran end-to-end scoring, not only
unit mocks. Nine Rust example tests and fifteen TypeScript tests passed after
review fixes. Attached red logs show order/outcome, mixed-output and handle-label
checks failing before fixes. A missing baseline regression also failed before
the baseline was generated; it is not a runtime behavior red.

Full gate results are appended after qualification. Architecture allowlist
unchanged. Live providers, actual bills, handle rates and production task quality
remain unverified. No CE-17 release gate or default change is claimed.

## Final qualification

The first full local runner passed 17/18 checks: 2,157 Rust tests across
243 binaries, 246 Client/React tests, 753 native/core tests, dependency,
architecture/API/contracts, fixture, TypeScript, lint, native/umbrella builds
and smoke all passed. Clippy initially rejected owned-string comparison.
The verifier now compares explicitly serialized byte vectors, because Value
equality ignores field order. No lint suppression or weaker comparison was
introduced. All nine example tests passed again; the corrected runner passed
both formatting and clippy (2/2). Thus every one of the 18 required checks
has a passing final result. `gates-initial.log` and `gates-corrected.log` retain
the exact summaries. Package source and frozen corpus/report were compared
byte-for-byte with master before staging. No production runtime source changed.
