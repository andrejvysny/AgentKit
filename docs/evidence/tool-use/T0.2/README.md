# Package T0.2: endpoint probe

Requirement: TU-20. Base: AgentKit `1bbe383` on `master`. OneMind untouched.

## What was built

- `agentkit-providers::probe`: checks P1 to P18 from the spec table, a JSON and
  Markdown report, and `Secret` (key holder, redacted `Debug`, `scrub`).
- `endpoint_probe` binary: `--base-url`, `--model`, `--out`, `--only`, `--runs`.
  The key comes from `PROBE_API_KEY` only, never from arguments.
- Talks raw HTTP with `reqwest`, not the provider adapters, so server quirks
  (reasoning field names, status codes, template handling) stay visible.

## Decisions

- Statuses: `pass`, `fail`, `info` (a measurement or accepted/rejected fact
  with no pass mark: P12, P13, P16), `error` (transport failure, never a pass).
- P11 passes at 5% or fewer failures over `--runs` (default 100). That
  threshold is my choice; the spec asks only to report the rate.
- P14 treats `/model/info` as optional (plain vLLM has none).
- Exit code is 1 on any `fail` or `error`, so a run is scriptable.

## Verification run

- `tests/endpoint_probe.rs` against a loopback mock that replays server
  behaviours: a compliant server passes all 18; each broken behaviour fails its
  check (no tool parser, corrupted text, stringly types, rejected extra params,
  no reasoning, dropped required calls); the key never appears in the report
  even when the server echoes it; an unreachable endpoint is `error`.
- `cargo clippy -p agentkit-providers --all-targets -D warnings`, tests,
  `check-arch` (allow-list unchanged), `public-api`, `check:api`: pass.

## Not verified

- No live run. The mock proves the checks and the redaction, not how vLLM or
  LiteLLM behave. Every `Probe` mark in the spec stays open until the owner
  runs the probe against the endpoint.
- The mock is written from the spec's description of the behaviours, not
  recorded from a real server; real fixtures can replace it after the live run.
