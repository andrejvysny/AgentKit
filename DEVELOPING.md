# Developing / consuming `agentkit`

AgentKit ships as **one** installable package, `agentkit`, with subpath
imports (`agentkit/core`, `agentkit/native`, …) backed by an
umbrella package (`packages/agentkit`) whose `dist/` is assembled from the
six `@agentkit/*` source packages by `scripts/build-umbrella.mjs`. See
[`packages/agentkit/README.md`](packages/agentkit/README.md) for the
subpath table.

Three workflows are supported, picked based on how much you need to
iterate:

| Workflow                     | When to use                                             | Setup cost                        | Speed of feedback              |
| ----------------------------- | -------------------------------------------------------- | ---------------------------------- | -------------------------------- |
| **Exact artifact install** (default) | You only consume `agentkit`; you don't edit this repo | none                                | seconds per artifact             |
| **`npm link`**                  | You're actively editing AgentKit and want a consumer to pick up changes | one-time `npm link` ritual          | rebuild + relink → next import  |
| **TypeScript path overlay**      | You're editing both repos at once and want IDE jump-to-source without a dist rebuild | drop-in `tsconfig.dev.json`        | instant (no build at all)        |

## 1. Exact artifact install

The prepared source version is not evidence of a published release. No installable
release tag was found during the 2026-10-06 audit. Do not pin a speculative
`v0.5.0` or use `#master` as an installable artifact: the source root is a
private workspace and does not contain the built umbrella at its root.

Build and pack the umbrella from a clean checkout, retain the tarball SHA-256,
then install that exact tarball:

```sh
npm install /absolute/path/agentkit-VERSION.tgz
# or
bun add /absolute/path/agentkit-VERSION.tgz
```

Node >=20 is the portable package engine. `agentkit/native` loads the prebuilt
platform addon; an Electron consumer must package a compatible addon and
validate its actual Electron runtime.

## 2. `npm link` (recommended for active development)

Use this when you're editing a `packages/<pkg>/src` file in this repo and
want the change reflected in a consumer without bumping a version and
re-tagging.

### One-time, from this repo's root:

```bash
bun install
bun run build            # every source package's dist/
bun run build:umbrella   # assembles packages/agentkit/dist from the above
cd packages/agentkit
npm link
```

Source installs use Bun's hoisted linker (`bunfig.toml`) so the native wrapper
and its platform packages resolve from the root `node_modules`.

### From the consumer:

```bash
npm link agentkit
```

After editing a package's `src/`, rebuild before the consumer sees it —
there is no watch mode across the umbrella assembly step:

```bash
bun run build && bun run build:umbrella
```

### Caveats

- `npm link` symlinks point at `packages/agentkit/dist`, which is a
  **generated copy**, not a live view of the source packages — you must
  re-run `bun run build && bun run build:umbrella` after every edit.
- Some bundlers (Vite, esbuild) cache resolved paths; restart the
  consumer's dev server after linking/unlinking.

## 3. TypeScript path overlay (fastest, IDE-friendly)

For an even tighter loop — no `dist/` rebuild needed, even for types — drop
a `tsconfig.dev.json` into your consumer that maps `agentkit/*` directly to
each source package's `src/index.ts`:

```jsonc
{
  "extends": "./tsconfig.json",
  "compilerOptions": {
    "paths": {
      "agentkit/contracts": ["../AgentKit/packages/contracts/src/index.ts"],
      "agentkit/client": ["../AgentKit/packages/client/src/index.ts"],
      "agentkit/react": ["../AgentKit/packages/react/src/index.ts"],
      "agentkit/core": ["../AgentKit/packages/core/src/index.ts"],
      "agentkit/testing": ["../AgentKit/packages/testing/src/index.ts"],
      "agentkit/native": ["../AgentKit/packages/native/src/index.ts"]
    }
  }
}
```

Adjust the relative path if the two repos aren't checked out as siblings.
This affects **type resolution only** — runtime resolution still goes
through `node_modules`, so pair it with `npm link` (workflow 2) if you want
the runtime behavior to follow too. Not committed by default — opt in per
developer.

## Releases

The umbrella package version is the lockstep artifact version (`0.8.0`).
Individual `@agentkit/*` source packages retain their development versions. The
TypeScript candidate-snapshot flow of the `0.6`/`0.7` releases (inverse patches,
`snapshot-release`, `qualify-candidates`) was removed with the TypeScript backend
([ADR 0023](docs/adr/0023-retire-typescript-backend.md)).

A release is the umbrella tarball plus the native platform packages, qualified
from the exact bytes: `bun run build && bun run build:umbrella`, then
`node rust/scripts/check-native-umbrella.mjs` and
`node rust/scripts/check-native-package.mjs`. Native packages ship through the
gated lockfile lane in `scripts/release/native-lockstep.mjs` and
`scripts/release/native-publish.mjs`. Nothing here tags, pushes or publishes.

## Local gates (no hosted CI)

Only local gates and qualification are enabled. Hosted GitHub workflows and
composite actions are absent; `bun run check:local-actions` rejects their
reintroduction. Historical definitions remain in Git history. No hosted
publication route is enabled.

Use the local qualification entry point with a new directory outside the
checkout:

```sh
bun run qualify:local /tmp/agentkit-hardening-candidate
```

The runner captures Git SHA, dirty patch hash, all nonignored source file
hashes (including untracked files), frozen source bytes, tool versions,
commands, exit codes, skips, logs, and artifact hashes. It refuses an existing
evidence directory and executes a separate copy of the captured source with
a fresh frozen dependency install. Edits to the original checkout cannot
change that execution copy. Changes to the execution copy's source invalidate
the qualification. Candidate
artifacts and final evidence are made read-only. It reuses the existing
packed-crate, native package, umbrella and artifact verifiers; it publishes
nothing. `qualify:cold` separately exercises disposable cold declaration
builds.

Results distinguish `PASS`, `FAIL`, `BLOCKED`, `NOT RUN`, and `INCONCLUSIVE`.
`--only gate,gate` runs a diagnostic subset; omitted gates remain `NOT RUN`.
Prerequisites must also be selected. A successful process exit in a source
suite does not establish full migration parity. Native host/store/runner
facades and managed-service capability gaps retain their own acceptance gates.

Current qualification scope is macOS only. Each invocation qualifies its
actual host ABI and installed Node/Bun versions. Other targets are `NOT RUN`,
outside that local scope; Linux and Windows qualification is deferred.
Missing configured runtime cells remain blocked. Required runtimes are Node
20, 22, 24, Bun 1.3.14, and the current supported Bun. Preserve macOS ARM64
and x64 evidence separately, and explicitly record Rosetta when used for x64
execution. Cross-builds without execution do not qualify a runtime. A local
pass does not establish release readiness or complete Rust migration parity.

Additional installed runtimes can exercise the same retained artifact without
repacking it. Supply absolute executable paths with `--node20`, `--node22`,
`--node24`, `--bun-minimum`, or `--bun-current-supported`. The runner checks
each version and pins those paths in the installed-artifact verifier. For example:

```sh
bun run qualify:local /tmp/agentkit-hardening-matrix \
  --node20 /opt/node20/bin/node --node24 /opt/node24/bin/node \
  --bun-minimum /opt/bun-1.3.14/bin/bun
```

The individual gates remain available for diagnosis:

```sh
# Rust workspace
(cd rust && cargo fmt --all --check \
  && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings \
  && cargo test --workspace --all-features --locked)
node rust/scripts/check-packages.mjs --offline

# Native binding
(cd packages/native && bun run build:native)
AGENTKIT_REQUIRE_NATIVE=1 bun test packages/native --timeout 60000
node rust/scripts/check-native-package.mjs

# TypeScript workspace
bun run lint && bun run typecheck
bun run build && bun run build:umbrella && node rust/scripts/check-native-umbrella.mjs
bun run check:api
bun test --timeout 60000
```

Other operating systems require separate authorization to extend qualification scope.
