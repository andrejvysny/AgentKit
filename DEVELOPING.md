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
Bun. An Electron consumer must rebuild/package the native addon for its own
Electron version and ABI, then validate that actual runtime.

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

The umbrella package version is the lockstep artifact version. The individual
`@agentkit/*` source packages retain their development versions. Release A is
an immutable migration foundation snapshot; Release B is a later snapshot that
adds Responses. Qualify each independently, using distinct source manifests,
artifact versions, and tarball hashes. Neither snapshot implies publication.

Create the snapshot outside the live checkout so active work cannot change its
build. Record the baseline Git commit, the dirty diff hash, all included source
file hashes, and any deliberately excluded files/exports. Apply exclusions only
to the snapshot. Compute its source digest after those edits and before building.
The snapshot helper records explicit exclusions, copies tracked and untracked
nonignored source, verifies source hashes during the copy, and changes umbrella,
wire contract, and lockfile workspace versions only in that copy. Release B uses
`0.7.0 responses` after continuation integration and final source validation.
The source digest excludes generated `dist`, dependency directories, and Git
metadata; it is not a substitute for recording the baseline commit and diff.

Create Release A only after all foundation source writers have frozen their changes:

```sh
node scripts/snapshot-release.mjs /tmp/agentkit-release-a 0.6.0 foundation
cd /tmp/agentkit-release-a
bun install --frozen-lockfile
bun run typecheck
bun test
node scripts/source-digest.mjs . /tmp/source-manifest.json
bun run build
bun run build:umbrella
mkdir -p /tmp/agentkit-candidate
npm pack ./packages/agentkit --cache /tmp/agentkit-npm-cache \
  --pack-destination /tmp/agentkit-candidate
node scripts/qualify-package.mjs \
  --tarball /tmp/agentkit-candidate/agentkit-VERSION.tgz \
  --source-digest SOURCE_MANIFEST_SHA256 \
  --output /tmp/agentkit-qualification \
  --electron 44.5.1
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
The optional exact `--electron 44.5.1` gate installs a clean Electron consumer,
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

`.github/workflows/release.yml` now performs candidate qualification with
read-only repository permissions on Linux and macOS, runs the Electron gate
on macOS, and uploads the exact tarball plus evidence. It
does not move tags, create release branches, publish packages, or claim a release
is accepted. Publication requires separate explicit authorization after evidence
review. A final install pin must identify an actual immutable, installable
artifact; source tags and prepared versions alone do not satisfy that gate.

For a Responses candidate, append `--responses true`. This adds clean-consumer
mock Responses host runs, tool dispatch, proposal auto-apply and rejection,
verification replay, and private continuation close/reopen checks. Append
`--migration-from /absolute/path/agentkit-0.6.0.tgz` to seed a populated schema-8
database using that exact foundation artifact, then migrate it with the candidate
to schema 9. Migration requires `--responses true`; both artifact digests and
fixture digests are recorded. The fake applier writes a fixture ledger only;
these checks do not qualify a PCB application or paid provider credentials.
