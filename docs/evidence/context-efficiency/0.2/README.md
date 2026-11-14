# Package 0.2: cache-simulating mock provider

Base: package 0.1, `21c1662801104be22884b475fe9b1f10470890b6` on master.
Requirement: CE-02 test infrastructure; enforcement belongs to package 1.8.

The dev-only `CacheSimulatingProvider` retains distinct unexpired requests
per caller-supplied cache key and chooses their longest common byte prefix.
Lifetime and minimum size are explicit parameters. Exact-request repeats
refresh that entry; expiry occurs at its deadline. Clock rollback clears
entries rather than serving stale hits. Time and identity use the existing
Runtime port, so tests need no sleeps or network.

The estimate is UTF-8 bytes divided by four, rounded down. This is a mock
model, not a provider tokenizer. Generic request serialization preserves
argument strings but is not provider wire representation. `WireObserver`
only records bytes supplied to it; package 0.3 connects actual assembly.
Known empty output, reasoning and cache-write counters are zero. Required
usage metadata and legacy totals make the mock work through real `run_chat`.

## Evidence

- Eight tests passed under Rust 1.97.0: hits, key isolation, changed prefix,
  expiry, minimum boundaries, exact prefix counts, A/B/A reuse, rollback,
  argument preservation and real loop event construction.
- Red evidence is in `red-retry.log` and `rollback-red.log`: A/B/A missed a
  warm entry, known zero output was absent, and rollback retained a stale
  entry before the corresponding fixes.
- Independent review found only missing red-log attachments; both are now
  included. The architecture allowlist is unchanged.
- Isolated-source gates passed 2,149 Rust tests across 241 binaries, plus
  formatting, clippy, machete, dependency audit, architecture, Rust API,
  fixture build, 246 Client/React tests, contracts, typecheck, native build,
  TypeScript build, 753 native/core tests and Node smoke.
- The first runner invocation passed 15/17 checks. Lint was rerun after
  copying the formatted native-budget correction from 0.1. The API check
  exposed a missing clean-build prerequisite: umbrella declarations had
  not been assembled. Building the umbrella, updating its intended usage
  API mirrors, and rerunning API and lint checks passed. The runner now
  explicitly builds the umbrella, for 18 gate checks. This corrects the
  stale-artifact dependency left by package 0.1.

Source copies allow overlapping qualification while master commits retain
package order. This package contains the cache module, tests, registration,
API/evidence and described gate correction. No production provider change,
live cache hit, actual paid bill or release qualification is claimed.
