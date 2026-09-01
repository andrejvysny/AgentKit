# ADR 0008 — Distribution: the `agentkit` umbrella package, and the adapters as products

**Status:** accepted, implemented (2026-09-01, recovery fixes 2026-09-01)
**Contract impact:** NONE. `CONTRACT_VERSION` stays `0.3.0` through every
commit in this ADR — everything here is packaging, distribution, and the
promotion of existing adapter code to published packages; no DTO gains or
loses a field. (The following wave, [ADR 0009](0009-content-parts-attachment-resolver.md),
is what bumps the contract to `0.4.0`.)

## Problem

AgentKit had no runnable path to being consumed anywhere. A fresh-context
inventory across this repo and both intended first consumers (OpenPCB,
OneMind — both single-process `bun:sqlite` apps) found:

1. The only `AssistantStore`/`TaskRunner` implementations lived under
   `internal/reference-adapters`, `private: true` — unpublishable by
   construction. Both target consumers need exactly the adapters marked
   unpublishable; there was no runnable composition root anywhere, only test
   fixtures.
2. Every cross-package dependency inside `packages/*` used `workspace:*`,
   which a GitHub-tag install — the only install path available while the
   `@agentkit` npm scope's ownership is unverified (see
   [`docs/non-goals.md`](../non-goals.md)) — cannot resolve.
3. `UsageAuthorizer` and `AuthorizationPort` existed as ports with zero
   framework call sites; retry had no backoff (OneMind's own runner backs off
   1s→30s in production).

## Evidence

- The originating plan's consumer inventory: OpenPCB and OneMind are both
  single-process `bun:sqlite` embeddings, so the adapters blocking adoption
  are precisely the ones `internal/reference-adapters` already implemented
  and could not ship.
- The sibling `shared/` repo's proven `release.yml` + `DEVELOPING.md` pattern
  (per-package tags, subtree-split release branches) — a working precedent
  for a monorepo shipping to consumers that do not build it from source,
  adapted here for one lockstep-versioned umbrella instead of many
  independently-tagged packages (see Decision 5 for why).
- The moment the adapters became a runnable, end-to-end composition root
  (`examples/desktop-host`, driving a real HTTP smoke test) rather than a set
  of unit-tested internals, a same-day fresh-context verifier pass found two
  CRITICAL recovery defects (see Decision 10) that no prior test had
  exercised — evidence that "does it actually run, wired end to end" surfaces
  a different class of defect than "do its unit tests pass."

## Decision

1. **One installable package, `agentkit`, unscoped.** `packages/agentkit`
   ships subpath exports over the twelve `@agentkit/*` source packages
   (`contracts`, `client`, `react`, `core`, `host`, `testing`, `mcp-client`,
   `transport-http`, `mcp-server`, `adapters-memory`, `adapters-sqlite`,
   `runner-local` — the last two of these added later in the wave). Unscoped
   rather than `@agentkit/agentkit` or `@openpcb/agentkit`: a GitHub-tag
   install ignores the registry namespace entirely, and scope ownership is
   exactly the thing not yet confirmed (see [`docs/non-goals.md`](../non-goals.md)).
   Subpaths rather than one flat root export, so a Node-only consumer's
   dependency graph never has to load `adapters-sqlite`'s `bun:sqlite` import
   just because the umbrella package exists.
2. **The build is a deterministic specifier rewrite, not a bundle.**
   `scripts/build-umbrella.mjs` runs every source package's own `tsc` build,
   copies each `dist/` under `packages/agentkit/dist/<pkg>/`, and rewrites bare
   `@agentkit/<pkg>` import specifiers to relative paths across both `.js` and
   `.d.ts` output, then asserts no `@agentkit/` scope string survives the
   rewrite. This is safe as a mechanical text transform — never a bundler —
   because every package exports only `"."` and every internal import already
   carries an explicit `.js` extension, so each specifier has exactly one
   unambiguous relative target.
3. **Release ritual: committed-dist release branches via `git subtree
   split`**, modeled on `shared/`'s workflow. Pushing tag `vX.Y.Z` triggers
   `.github/workflows/release.yml`: typecheck, test, build every source
   package, `build:umbrella`, `smoke:umbrella` (installs the built tarball
   with **both** npm and bun and imports every subpath), commit the built
   `packages/agentkit/dist` into a throwaway commit on the tagged tree,
   `git subtree split --prefix=packages/agentkit -b release/vX.Y.Z`, force-push
   that branch, then re-point the tag at the split commit. A consumer's
   `"agentkit": "github:owner/AgentKit#vX.Y.Z"` therefore resolves a repo root
   that **is** `packages/agentkit` with `dist/` already committed — no
   `prepare` script, no build step for the consumer, works under npm and Bun
   alike. The release branch and the tag are **force-moved** on every run for
   that tag; neither is a stable ref to branch off.
4. **Lockstep version.** `packages/agentkit/package.json`'s `version` is the
   one release version for the whole `agentkit` package; the individual
   `@agentkit/*` packages under `packages/*` keep independent `-dev` versions
   and are never tagged or released on their own. `release.yml` asserts the
   pushed tag matches that version **exactly** — a mismatch fails the release
   outright rather than adjusting the tag (e.g. stripping `-dev`) to fit.
5. **Why GitHub-tag beats per-package tags — the consumer version-drift
   class of bug.** If each of the twelve packages were independently
   versioned and tag-installed (the way `shared/`'s registry-published
   packages are), a consumer's lockfile could pin `adapters-sqlite@v0.3.0`
   against `host@v0.4.0` — two packages built against different, possibly
   incompatible port shapes, with nothing to catch the mismatch at install
   time (a git-tag install has no registry and no `peerDependencies`
   resolution). Lockstep plus one umbrella version collapses "which
   combination of twelve packages is compatible" to "which single version" —
   a question with one obviously right answer, at the one moment (before any
   external consumer exists) this is cheapest to decide.
6. **Why not npm.** The `@agentkit` npm scope's ownership is not yet
   verified (see [`docs/non-goals.md`](../non-goals.md)); publishing under an
   unowned scope is not attempted. Every package's `publishConfig` is kept
   ready (from [ADR 0006](0006-hardening-tranche.md)'s packaging pass) for the
   day scope ownership is confirmed, but the GitHub-tag umbrella is the
   **entire** distribution story until then — a complete answer to "how does
   a consumer install this," not a stopgap standing in for npm.
7. **`internal/reference-adapters` is promoted into published packages**:
   `@agentkit/adapters-memory`, `@agentkit/adapters-sqlite` (Bun-only), and
   `@agentkit/runner-local`. This is a **stance change**, not just a move:
   `adapters-sqlite` is no longer documented as a reference implementation for
   tests — it is now **the production `AssistantStore` for a single-process
   host** (a desktop app, a CLI, a sidecar), carrying forward the
   multi-handle-file guarantees [ADR 0006](0006-hardening-tranche.md) proved.
   It **owns its own database file** (no table prefix — a dedicated file
   avoids every collision with a consumer's own schema or migrator outright,
   at zero cost, where a prefix scheme buys nothing until a real consumer asks
   to share a file) and ships **no migrations, by design**: `PRAGMA
   user_version` fails closed — a database written by a different schema
   version is refused, and a stale *dev* database is recreated rather than
   upgraded in place. A migration story is only meaningful once more than one
   shipped schema version is running in a consumer's production database,
   which is not yet true for a package with no external consumer yet.
8. **Retry backoff added to `SingleProcessTaskRunner`**: exponential and
   jittered — `min(baseMs * 2^(attemptCount - 1), maxMs)`, spread by
   `± jitterRatio` (defaults 1s base / 30s ceiling / 0.2 jitter,
   configurable per runner). The deadline is tracked in the **runner**, never
   the store: a task mid-backoff is still `running`, still leased and still
   holding its scope, so a crash during the wait recovers exactly like a
   crash during the attempt, and the delay is measured against the injected
   `Clock` so a test drives it rather than sleeping.
9. **`describeTaskRunnerConformance` added to `@agentkit/testing`**,
   framework-neutral and run against both reference stores — the `TaskRunner`
   port's behavioral bar, the counterpart to what
   `describeAssistantStoreConformance` already is for storage.
10. **Two CRITICAL findings, closed same-day, both consequences of making
    this a runnable composition root for the first time:**
    - **Landed-gated lease release.** `AttemptOutcome` gained a `landed:
      boolean`. The lease is released only when the execution is actually
      *finished* with it — a terminal outcome, or the lease already moved to
      another owner — never on a deliberate walk-away (a shutdown arriving
      mid-retry), because in that case the live lease is the **only** thing a
      later `recover()` can find the task by; releasing it would leave a task
      `running` with no lease, unclaimable and unrecoverable.
    - **`pendingRedispatch`.** `recover()` is documented safe to call before
      `startWorker` — the boot order every host wires. But by the time a
      workerless `recover()` pass discovers there is nobody to hand work to,
      `expireStaleLeases` has already *deleted* the very lease a later
      `recover()` would have found the task by. Abandoned tasks are now
      parked in a `pendingRedispatch` set and drained the instant
      `startWorker` (or a later `recover()`) runs, closing the gap between
      "recovered" and "findable again."
    - **Post-backoff fencing.** `settleThrown` now re-checks
      `stillHoldsLease` *after* the backoff wait completes, before minting a
      new attempt. A backoff can outlast the lease TTL (the heartbeat during
      it is best-effort — a store that is briefly unreachable is exactly the
      failure being backed off from); if the lease already moved to another
      owner's recovery attempt during the wait, starting a fresh attempt here
      would steal the task back and run it **concurrently** with that
      recovery. The fix: write nothing, touch nothing — the task has an owner
      and it is not this one.

## Alternatives considered

- **A bundler (esbuild/rollup) for the umbrella build.** Rejected: twelve
  packages that already build clean ESM via `tsc`, each already carrying
  explicit `.js`-extensioned internal imports, is exactly the case where a
  bundler adds risk (minification edge cases, sourcemap and dual-package
  hazards) without buying anything a deterministic specifier rewrite does not
  already give for free.
- **Per-package semver with independent GitHub tags**, mirroring `shared/`'s
  per-package tag scheme. Rejected: `shared/`'s consumers install from the npm
  registry, where `peerDependencies`/version ranges are enforced at install
  time; a git-tag install has no such enforcement, so independent per-package
  tags would reintroduce exactly the version-drift class of bug lockstep
  exists to prevent — a `shared`-shaped answer that only works because
  `shared` is on the registry.
- **Keep `internal/reference-adapters` private and let each consumer write
  its own store.** Rejected: this was the state the originating inventory
  named as the top blocker to adoption — both target consumers are
  single-process `bun:sqlite` apps needing exactly this adapter, and asking
  each to reimplement lease/fencing/CAS transitions duplicates the
  hardest-won code in the repository.
- **A table-prefix scheme so `adapters-sqlite` could share a consumer's
  existing database file.** Rejected: a dedicated file avoids every future
  collision with a consumer's own schema or migrator outright, at zero
  ongoing cost; a prefix scheme buys nothing until a real consumer actually
  asks to share one file, and none has.

## Consequences

- AgentKit is installable, end-to-end, by a consumer that has never seen this
  repository's workspace: `"agentkit": "github:andrejvysny/AgentKit#v0.4.0"`,
  `npm install`, done — no build step, no `workspace:*` to resolve.
- `examples/desktop-host` is now the canonical embedding recipe: the
  composition root every later design question ("how would a host actually
  wire this?") can point at, rather than describe hypothetically.
- `@agentkit/adapters-sqlite` carries production-store expectations from this
  point on — its multi-handle-file guarantees ([ADR 0006](0006-hardening-tranche.md))
  are load-bearing, not incidental test infrastructure.
- The npm-publish entry in [`docs/non-goals.md`](../non-goals.md) is now
  narrower: GitHub-tag distribution is a complete, working answer on its own,
  not a placeholder waiting on npm scope ownership — nothing about adoption
  is blocked on npm publish any more, even though publish itself remains
  gated.
- Both CRITICAL recovery findings are now permanent invariants the
  `runner-local` test suite pins: a lease-renewal scenario that specifically
  kills a no-op `renewLease`, and jitter/backoff tests rewritten to be
  mutation-killing.
- `docs/roadmap.md`'s P1–P4 "Done" phases predate this ADR and are unaffected
  by it; this ADR is itself recorded as newly Done work in the same file.

## Out of scope (deliberate)

Publishing to npm itself (still gated on `@agentkit` scope ownership, see
[`docs/non-goals.md`](../non-goals.md)); a table-prefix or shared-database-file
scheme for `adapters-sqlite`; distributed adapters over a networked backend
(Postgres, Redis) — this ADR promotes the single-process reference adapters
to published packages, nothing more.
