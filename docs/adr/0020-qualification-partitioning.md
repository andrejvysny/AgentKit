# ADR 0020 — Qualification partitioning: TypeScript runtime, native, and platform gates

**Status:** accepted, implemented (2026-10-28; Phase A of the stabilization tranche).
**Contract impact:** none. CI topology and test-fixture preparation only.

## Problem

One TypeScript workflow carried every gate, and its failures conflated three
unrelated causes: macOS sandbox (`sandbox-exec`) behaviour, native (Rust)
fixture preparation, and the TypeScript agent runtime itself. The last red
master run mixed 3 coding-agent acceptance failures with 10 Rust interop
fixture failures — reviewers could not tell "the runtime broke" from "the
runner lacks a dependency" or "the sandbox behaves differently".

The two concrete defects behind the mixing:

1. The shared native fixture builder (`rust/tests/support/http-server.ts`)
   ran Cargo in `--offline` mode unconditionally, so any dependency missing
   from the runner's local cache failed the *TypeScript* suite.
2. The coding-agent acceptance suite ran macOS-only sandbox behaviour and
   cross-platform logic in one job, on a Linux runner.

## Decision

- **Native fixtures are explicit setup.** Dependency resolution happens in a
  permitted setup phase; the fixture is built once; interop tests run against
  the built fixture; `--offline` is used only after availability was
  established; preparation failure reports an infrastructure diagnosis, never
  a runtime-semantics workaround. Rust interop moved to the Rust workflow —
  TypeScript CI no longer builds native code.
- **Platform-correct gates.** Tests that require the real macOS sandbox run
  on a macOS runner (`macos-sandbox` job) instead of Linux skips counting as
  evidence. Cross-platform runtime and permission tests stay in the normal
  TypeScript CI. Production sandbox defaults stay fail-closed; any
  non-sandboxed test mode exists only inside explicitly isolated fixtures that
  cannot weaken production behaviour.
- **Qualification areas are separate jobs** (TypeScript unit, Node/Bun
  compatibility, memory/SQLite conformance, durable recovery, coding-agent
  acceptance, macOS sandbox, Rust interop, packaging/umbrella smoke,
  security-sensitive regressions) so one red job names one cause.

## Consequences

- A red TypeScript job means a TypeScript problem; a red Rust job means a
  native problem. Diagnostics for stalled/crashed acceptance children now
  capture stderr and process state instead of timing out silently, and the
  SIGKILL recovery acceptance asserts durable store state, not elapsed time.
- The macOS sandbox job is the only place sandbox behaviour is qualified;
  losing that runner is a visible qualification gap, not a silent skip.
