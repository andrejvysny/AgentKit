# Embedding AgentKit in OpenPCB

This guide describes the supported framework seams. Acceptance and sequencing live in the
[canonical Plane plan](https://app.plane.so/andrejvysny/projects/9b59c292-53a4-4887-948b-26531650ac60/pages/7172dcf4-9299-402a-b837-87f60244f488/).
It supersedes the historical Bun-only recipe, legacy-chat import, automatic recovery and
unconditional Tasks module deletion instructions. No application cutover is implied.

## Runtime and installation

Published consumer contracts were inspected at
[OpenPCB d69189fa88605e140af01c1246e2da54b04f2201](https://github.com/OpenPCB-app/OpenPCB/commit/d69189fa88605e140af01c1246e2da54b04f2201).
Its authoritative `package-lock.json` pins Electron **41.6.1** and better-sqlite3 **12.10.0**;
the Electron manifest requests `^12.10.0`. Its `bun.lock` is historical. AgentKit currently
requires better-sqlite3 `^13.0.3` for Node storage. Do not suppress peer errors or assume the
application’s existing driver satisfies this requirement. Consumer dependency changes belong
in OpenPCB and require its own application tests.

The agreed migration target is Electron **41.6.1** with better-sqlite3 **13.0.3**.
OpenPCB's owner updates the driver and validates the application; an Electron major upgrade
is not a prerequisite.

The backend runs inside Electron's main Node process. Keep that architecture: no Bun sidecar,
framework build during installation or sibling checkout at runtime. Electron main is a CommonJS
bundle; bundle AgentKit's JavaScript and keep `better-sqlite3` external. This does not promise
direct `require('agentkit')` compatibility.

Foundation 0.6.0 contains the migration runtime and schema 8. Responses 0.7.0 adds the provider
and schema 9 continuation storage. These separate candidate tracks do not make foundation wait
for Responses, OAuth or SIWC approval. Source versions and commits are not package releases.

Follow the [release procedure](../../DEVELOPING.md#qualifying-a-release-candidate): snapshot,
build and pack once, then qualify those exact bytes. Install only the reviewed versioned tarball
after checking its recorded SHA-256. Retain the consumer lockfile and archive integrity. Never
use mutable `master`, a workspace symlink or consumer-side framework build as the production
install target. A local candidate path is not a remotely available release.

Qualification records actual runtime versions, architecture, native binary path/digest, database
reopen, browser import boundaries and externalized CommonJS bundle behavior.
`ELECTRON_RUN_AS_NODE=1` proves that native runtime path. It does not qualify packaged GUI,
installer resolution, keychain identity or source recompilation. Those are OPENPCB-116 gates.

## Composition and lifecycle

Use the executable [Node desktop host](../../examples/desktop-host/node-host.mjs) and its
[instructions](../../examples/desktop-host/README.md). Compose one managed
`NodeSqliteAssistantStore` from `agentkit/adapters-sqlite-node`, one local task runner,
`TurnRunner`, executor registry, REST handler and application-owned ports.

Open a dedicated `APP_DATA_DIR/agentkit.sqlite`. Never pass `openpcb.sqlite` to AgentKit's schema
manager. Initialization errors must not delete either database, WAL or SHM. The
[SQLite guide](../../packages/adapters-sqlite/README.md) defines supported upgrades,
foreign/newer database refusal, WAL backups and ownership limits. Legacy chat import is outside
this migration; designs, library data and settings are retained.

Set `recoveryMode: "manual"` and `shutdownMode: "cancel"` on `SingleProcessTaskRunner`.
Recover before starting the worker. Boot may reconcile existing receipts but starts no model
requests or new domain writes. Explicit resume retains task identity and cumulative budgets.
Shutdown rejects new submissions, cancels work, stops streams/workers, then closes storage.
See [execution and settlement](../execution-lifecycle.md).

Use one feature-selected writer per conversation. Compare old/new behavior in separate chats
and cloned design fixtures; never run both mutation paths against one live design. The Tasks
monitor and remaining TasksSDK consumers need explicit migration decisions before removing
old routes, generated SDKs or the Tasks module.

## Application-owned credentials

OpenPCB owns OAuth, OS encryption, typed credential IPC, rotation and historical-reference
retention. Its [credential contract](https://github.com/OpenPCB-app/OpenPCB/blob/d69189fa88605e140af01c1246e2da54b04f2201/docs/assistant/credentials.md)
uses opaque versioned references. Rotation creates a new reference; pinned runs retain access
to old references. Renderer code can set, clear and query status, but cannot retrieve saved keys.

AgentKit's generic provider REST lifecycle differs: `providerSecretRef` derives
`provider/<providerId>/api-key`, updates overwrite that reference, and provider deletion deletes
the referenced secret. Do not mount these mutations over OpenPCB's retained-reference vault.
Keep credential mutation in application IPC. Filter provider writes before the REST handler,
or deny the `provider` resource's `write` action with `RestHandlerDeps.authorize`.
Route application-owned provider management separately when configuration changes are needed.

The existing trusted `TurnRunnerDeps.providerFactory` is the embedding seam; no second vault
or credential API is needed. It receives provider configuration and returns a client. Inject a
`SecretStore` adapter with an opaque `metadata[PROVIDER_SECRET_REF_KEY]`, or let the injected
client resolve credentials privately at dispatch. Do not supply a writable vault to the generic
REST handler merely to support execution. A configured reference that cannot resolve must fail
explicitly in the application adapter, never become an unauthenticated request. No-key local
providers must remain usable without the keychain.

`TurnRunner` resolves ordinary provider configuration when an attempt starts, not when its task
is queued. A mutable provider ID alone does not pin the old endpoint, model or credential
version. For submit-time pinning, the trusted application must persist an immutable nonsecret
provider registration per generation, submit its explicit ID and model, and retain it while a
run can resume. Renderer-supplied references must not select arbitrary vault entries. Validate
this adapter against OPENPCB-115's queued-run rotation/deletion fixtures. Responses clients also
freeze their trusted connection identity; see the [Responses protocol](../providers/responses.md).
Foundation does not require that adapter.

## Provider and domain ports

Preserve OpenAI, OpenRouter, custom compatible endpoints, LM Studio and oMLX independently of
cloud entitlements. Honor application tool overrides and selected models. Missing keys,
disabled providers, unavailable models and local connection failures remain explicit; never
switch payment sources or providers automatically. Apply consumer remote/local budgets
centrally; do not reset them for each provider pass.

Tool bodies, context/mentions, proposal apply/undo, verification, design bindings and risk policy
stay in OpenPCB. AgentKit supplies orchestration and durable publication, not PCB algorithms.
Non-destructive auto-apply and destructive approval use application policy. Crash-after-commit
recovery needs a durable domain receipt; task fencing alone cannot prove exactly-once mutation.
Cancellation does not undo committed design changes.

Inbound MCP has its own authenticated actor/session boundary and registered domain catalog.
Do not expose account, credential or inference controls as domain tools. No outbound MCP
expansion is needed for migration or Responses.

## Consumer acceptance

The pinned consumer's [catalog fixtures](https://github.com/OpenPCB-app/OpenPCB/tree/d69189fa88605e140af01c1246e2da54b04f2201/src/core/backend/tests/fixtures/assistant-parity)
are published. `catalog.current.json` canonical-data SHA-256 is
`a64bdf2045794bd54b250887d6b6af2c4d42e16355e9681b0a41723325aefb01`.
It captures 15 in-app tools with actual schemas. Compare these definitions unchanged rather
than substituting approximate synthetic schemas. Sanitized fixtures may be vendored with pinned
provenance; OpenPCB must not become an AgentKit runtime dependency.

Framework acceptance covers schema preservation, call/result identity, continuation, usage,
cancellation, late failures, replay and final settlement through real host code with mocks.
A host-executed stub is not native PCB mutation, receipt or undo acceptance. OPENPCB-117/118 own
that proof; OPENPCB-116 owns packaged startup and cleanup. Foundation publication remains
independent of the full ChatGPT integration.
