# Package 0.1: normalized usage record

Qualification: passed. This note accompanies the package 0.1 source change.

Base: AgentKit `cf5dface4d7e8b7a8e24c18fb6c84b855ead7522` on `master`.
OneMind remains unchanged at `51098c671894eda6960d4448e3df2c27f67092a4`.
Requirement: CE-16, AgentKit runtime portion. The OneMind Anthropic adapter
and UI remain outside this package and unverified.

## Reference verification and decisions

The cleaned `chat`, `sessions`, `agents`, `engine`, `runtime`, `providers`
and `governance` crates exist; there is no `host` crate. Both working trees
were initially clean. Provider usage mapping remains in
`providers/src/chat_completions.rs` and `providers/src/responses.rs`.
The Claude driver separately parses vendor usage in `claude/src/protocol.rs`;
its generic executor previously emitted only native events.

The Rust DTO, Rust-owned schema bundle, TypeBox mirror, fixtures and public
API snapshots are updated together. Fields are additive, so contract version
`0.7.0` stays unchanged under the existing version policy.

Unknown counters remain absent rather than being reported as zero. Normalized
`output` excludes reported reasoning, while legacy completion totals keep it.
Original provider usage is retained. Cache-write lifetimes are assigned only
when the provider explicitly reports the split. An unsplit native aggregate
remains available in raw and legacy usage without a guessed lifetime.

Claude-specific interpretation stays in the driver. The native executor
projects its typed counters, preserves native events, merges partial counter
updates without discarding previously known values, and emits one final
usage record per attempt after draining updates. An empty usage observation
does not become a billable request. Legacy totals remain populated when their
components are known, preserving session-report consumers.

## Verification

- Provider TypeScript mapping regressions failed before implementation;
  subsequent focused mapping and validation tests passed (11 tests).
- Contract schema parity passed (131 tests); workspace TypeScript checking passed.
- Rust Chat Completions and Responses fixture checks passed (2 tests).
- Parent regression checks exposed duplicate native request counting and
  missing legacy totals (2 failed); a further regression exposed partial
  updates discarding input counters. After correction, all 5 focused native
  usage tests passed, including absent usage and repeated updates.
- A full DTO roundtrip test exposed rejection of integral JSON numbers such
  as `0.0`; the decoder was corrected to accept non-negative safe integers
  independent of their JSON spelling. All 14 contract parity tests passed.
- After the final partial-update correction, all 47 native-executor tests
  passed; the targeted five usage regressions also passed.
- Initial architecture check passed with unchanged allowlist counts:
  R1b 3, R5-glob 32, R5-path 31, R5-static 36, R11 1, R12 7.
- The workspace Rust gate passed 2,140 tests across 240 binaries. Its 12
  existing ignored tests remain unverified. Final workspace clippy passed.
- All 17 local gate checks passed across the runs described below. The
  architecture allowlist is unchanged.

Rust fixture tests were authored before the initial implementation, but their
first execution was delayed by a shared Cargo target lock. Do not interpret
that as observed failing-before evidence. Parent native regression failures
were executed and captured separately.

## Gate corrections and decisions

The first `node scripts/gates.mjs` invocation passed 14 of 17 checks.
Existing gate logs were backed up outside the checkout before that run.
The three failed checks were corrected and rerun:

- Client/React: a parallel source-copy build contended for the shared Cargo
  target, timing out the fixture fallback build. The rerun used the retained
  prebuilt HTTP fixture through `AGENTKIT_NATIVE_FIXTURE`: 246 tests passed.
  Its SHA-256 is
  `a1f2cfcc0865ebf0095caef200e6b73d4e28b71367096a184331ab713b219bb2`.
  Further qualification copies use separate targets to avoid this conflict.
- Lint: the independently prepared package 0.5 fixtures had callback-return
  and JSON-format errors. Those were corrected without staging package 0.5
  in this package. Repository lint passed with four existing warnings and
  two informational diagnostics.
- Native: the runner omitted the documented 60-second Bun budget. Two ABI
  tests exceeded its default five seconds; the isolated escape-hatch test
  required about 24 seconds. The gate now uses `--timeout 60000`, matching
  `DEVELOPING.md`. With process-inspection access, all 753 native/core tests
  passed. A sandboxed retry failed process-start witnesses because the outer
  sandbox denies process inspection; those are not successful native checks.

Formatting, clippy, Rust tests, machete, dependency audit, architecture, Rust
API, fixture build, contract parity, TypeScript checking, native build,
TypeScript build, TypeScript API and Node smoke passed. The corrected
Client/React, lint and native runs above complete the 17 checks; this is not
a claim that the initial nonzero invocation was green.

The initial `bun run check:local-actions` failed on the pre-existing tracked
`.github/workflows/ci.yml`, contradicting the disabled-hosted-CI policy.
The owner approved its removal in package 0.1. The workflow was removed,
the local-gate runner's stale CI comment was corrected, and
`bun run check:local-actions` now passes.

## Unverified

No paid provider call, model download, live Claude invocation, OneMind build,
push, tag, release, publication or Plane write was performed. Recorded Claude
fixtures are regression evidence, not a new live-provider qualification.
