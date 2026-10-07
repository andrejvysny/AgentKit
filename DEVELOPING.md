# Developing / consuming `agentkit`

AgentKit ships as **one** installable package, `agentkit`, with subpath
imports (`agentkit/host`, `agentkit/adapters-sqlite`, …) backed by an
umbrella package (`packages/agentkit`) whose `dist/` is assembled from the
twelve `@agentkit/*` source packages by `scripts/build-umbrella.mjs`. See
[`packages/agentkit/README.md`](packages/agentkit/README.md) for the
subpath table.

Three workflows are supported, picked based on how much you need to
iterate:

| Workflow                     | When to use                                             | Setup cost                        | Speed of feedback              |
| ----------------------------- | -------------------------------------------------------- | ---------------------------------- | -------------------------------- |
| **Exact artifact install** (default) | You only consume `agentkit`; you don't edit this repo | none                                | seconds per artifact             |
| **`npm link`**                  | You're actively editing AgentKit and want a consumer to pick up changes | one-time `npm link` ritual          | rebuild + relink → next import  |
| **TypeScript path overlay**      | You're editing both repos at once and want IDE jump-to-source without a dist rebuild | drop-in `tsconfig.dev.json`        | instant (no build at all)        |

## 1. Exact candidate artifact install

The prepared source version is not evidence of a published release. No installable
release tag was found during the 2026-10-06 audit. Do not pin a speculative
`v0.5.0` or use `#master` as an installable artifact: the source root is a
private workspace and does not contain the built umbrella at its root.

Build and pack an isolated source snapshot, retain its source manifest and tarball
SHA-256, then install that exact tarball:

```sh
npm install /absolute/path/agentkit-VERSION.tgz
# or
bun add /absolute/path/agentkit-VERSION.tgz
# Node SQLite consumers also install the optional native peer explicitly:
npm install better-sqlite3@13.0.3
```

The Node SQLite subpath requires Node >=22 because of its native peer. The
portable package engine remains Node >=20; the Bun SQLite subpath still requires
Bun. An Electron consumer must package a compatible native addon and validate its
actual Electron runtime. Rebuild from source only when the selected addon requires it;
the qualified driver 13.0.3 uses a Node-API prebuild.

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

Source installs pin `node-gyp` 10.3.1 and use Bun's hoisted linker. On fresh
Bun 1.3.14 and 1.4.0 workspace installs, the isolated linker can start the native
peer's implicit build before the local build tool is available, fall back to
cached `bunx node-gyp`, and remove the failed optional driver while returning
success. The checked-in install configuration avoids that order; required Node
SQLite tests still prove the driver is installed and loadable. Clean package
consumers are qualified separately with their own lockfiles.

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
      "agentkit/host": ["../AgentKit/packages/host/src/index.ts"],
      "agentkit/testing": ["../AgentKit/packages/testing/src/index.ts"],
      "agentkit/mcp-client": ["../AgentKit/packages/mcp-client/src/index.ts"],
      "agentkit/transport-http": [
        "../AgentKit/packages/transport-http/src/index.ts"
      ],
      "agentkit/adapters-memory": [
        "../AgentKit/packages/adapters-memory/src/index.ts"
      ],
      "agentkit/adapters-sqlite-node": ["../AgentKit/packages/adapters-sqlite/src/node.ts"],
      "agentkit/adapters-sqlite": [
        "../AgentKit/packages/adapters-sqlite/src/index.ts"
      ],
      "agentkit/runner-local": [
        "../AgentKit/packages/runner-local/src/index.ts"
      ]
    }
  }
}
```

Adjust the relative path if the two repos aren't checked out as siblings.
This affects **type resolution only** — runtime resolution still goes
through `node_modules`, so pair it with `npm link` (workflow 2) if you want
the runtime behavior to follow too. Not committed by default — opt in per
developer.

## Qualifying a release candidate

The umbrella package version is the lockstep artifact version. Individual
`@agentkit/*` source packages retain their development versions. Foundation
`0.6.0` and Responses `0.7.0` are separate, unpublished candidate tracks.
Foundation does not wait for Responses tests or provider integration acceptance.

Both are reproducible from the current integrated Git source. The snapshot helper
copies tracked and untracked nonignored regular files, detects source and Git
changes during the copy, and records each original file hash. Foundation applies
the reviewed inverse patches in `scripts/release/foundation/` from that captured
copy and removes the explicitly listed Responses and private-continuation files.
It retains current generic stream, storage, budget, fencing, and recovery fixes,
uses schema 8, and has no production v8-to-v9 migration. Responses retains the
integrated implementation and schema 9. No temporary prior snapshot is an input.
Patch drift fails closed: review and update the affected patch rather than restoring
an old file or weakening the check.

Each snapshot records its baseline commit, dirty diff hash, original file hashes,
requested exclusions, present exclusions with hashes, patch hashes, and changed-file
before/after hashes. Only the copy's umbrella, wire contract, workspace lock metadata,
and golden trace versions change. The source manifest includes projection metadata
and patches; it excludes generated dist, dependencies, Git metadata, and build info.
Its digest supplements, rather than replaces, the Git and transformation evidence.

Choose a new directory outside the checkout. Foundation qualification runs only its
own projected source and exact package artifact:

```sh
CANDIDATE_TRACK=foundation node scripts/release/qualify-candidates.mjs \
  /tmp/agentkit-foundation-candidates
# On macOS, add the exact Electron native runtime gate:
CANDIDATE_TRACK=foundation ELECTRON_VERSION=41.6.1 \
  node scripts/release/qualify-candidates.mjs /tmp/agentkit-foundation-electron
```

For Responses, select `CANDIDATE_TRACK=responses`. That track first qualifies a fresh
foundation artifact, then independently builds and packs `0.7.0` and uses the exact
foundation bytes for populated schema-8-to-9 migration qualification. Each source
is frozen and archived before its build, checked again afterward, and packed once.
The output retains source manifests, provenance, source archives, package tarballs,
source gates, package qualification, runtime logs, consumer locks, and bundle metafiles.
The helper never tags, pushes, publishes, or repacks a supplied artifact.

To inspect or build a source projection separately:

```sh
node scripts/snapshot-release.mjs /tmp/agentkit-release-a 0.6.0 foundation
node scripts/snapshot-release.mjs /tmp/agentkit-release-b 0.7.0 responses
```

Use the `sha256` value from `source-manifest.json`, not the hash of that JSON
file. The qualification command never repacks the supplied artifact. It installs
that same tarball in two new consumers with npm and Bun; checks published imports
and declarations with `npx tsc`; bundles browser graphs; bundles a CommonJS host
with `better-sqlite3` external; loads the installed native addon, writes and
reopens SQLite; and drives the Node fake-provider host over HTTP/SSE through
shutdown and reopen. The Bun consumer explicitly trusts the installed native
peer's lifecycle script; it never edits `node_modules` manually.

The retained evidence includes artifact/source digests, runtime versions,
platform/architecture, Node ABI, commands and logs, dependency lockfiles, and
bundle metafiles. CommonJS bundling under Node alone is not an Electron runtime qualification.
The optional exact `--electron 41.6.1` gate installs a clean Electron consumer,
executes its actual binary with `ELECTRON_RUN_AS_NODE=1`, loads the external
native addon from the CommonJS bundle, and reopens its database before and after
`npm rebuild better-sqlite3`. It records Electron/Node versions, module ABI,
Node-API version, loaded binary paths, and binary hashes. Driver 13.0.3 ships
Node-API 10 prebuilds with `gypfile: false` and no install lifecycle script:
that npm rebuild is a lifecycle check, not a source/ABI recompilation claim.
This qualifies that Electron native runtime in RunAsNode mode, not a GUI,
renderer, signed installer, or arbitrary Electron version. See
[Electron native module guidance](https://www.electronjs.org/docs/latest/tutorial/using-native-node-modules). No Electron GUI, PCB domain,
OAuth provider, paid credential, or restricted DevKit is involved.

`.github/workflows/release.yml` is a read-only manual qualification workflow.
Its `track` input defaults to `foundation`; selecting `responses` adds Responses
and exact foundation migration checks. Linux and macOS qualify with Node 22.23.2
and Bun 1.4.0; macOS additionally runs Electron 41.6.1. A separate required Bun
1.3.14 source gate validates only the selected track. Platform jobs use
`fail-fast: false`, and acceptance rejects failed, cancelled, or skipped jobs.
Failure logs and partial evidence remain available. Qualification creates no
release tags or externally installable package release.

Foundation publication requires its own successful `foundation` track run; the
foundation seed inside a Responses-track run is migration evidence only.

The prepared publication route is `.github/workflows/publish.yml`; it publishes
GitHub release assets, not an npm registry package. Before any dispatch, obtain
explicit approval for the destination, candidate version, source commit, source
manifest digest, exact tarball SHA-256, qualification run, and retained macOS
artifact ID. Configure the `agentkit-publication` environment with required
reviewers; existing self-review policy remains the repository owner's choice.
The verifier refuses an environment without configured required reviewers.

Publication accepts only a successful manually dispatched qualification run from
this repository's default branch and the corresponding selected-track artifact.
It downloads that immutable artifact ID and checks source archives/manifests,
projection provenance, clean Git evidence, exact package bytes/version, all source
and package checks, actual Node/native and Electron runtime evidence, and the
populated migration proof when publishing Responses. It requires the Bun minimum
and both platform jobs to have passed. No checkout build or package repack occurs.

Approval creates a new `vVERSION` tag pointing to the recorded source commit and
attaches the exact qualified tarball, SHA-256, frozen source, and evidence to a new
GitHub release. A projected foundation source tag still names integrated source;
install the release's tarball asset, not its source archive. Existing tags, releases,
or assets are never replaced. A partial publication failure requires manual review;
the workflow does not delete a created tag or reuse an existing release automatically.
The final install pin must identify the actual immutable release asset and its digest.
Versions and local candidate paths alone are not remote install pins. Neither
prepared workflow authorizes publication without the explicit approval above.

For a Responses candidate, append `--responses true`. This adds clean-consumer
mock Responses host runs, tool dispatch, proposal auto-apply and rejection,
verification replay, and private continuation close/reopen checks. Append
`--migration-from /absolute/path/agentkit-0.6.0.tgz` to seed a populated schema-8
database using that exact foundation artifact, then migrate it with the candidate
to schema 9. Migration requires `--responses true`; both artifact digests and
fixture digests are recorded. The fake applier writes a fixture ledger only;
these checks do not qualify a PCB application or paid provider credentials.
