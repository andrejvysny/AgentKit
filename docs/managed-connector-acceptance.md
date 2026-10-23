# Managed connector host acceptance

`examples/desktop-host/src/managed-connector-acceptance.ts` provides an opt-in
acceptance driver over `ConnectionService`. It uses the production discovery,
validation, authorization, session, and receipt paths. It creates no agent loop.
The desktop recipe re-exports `createManagedConnectorAcceptance`.

## Prerequisites

Use a caller-owned `ConnectionStore`, secure `SecretStore`, durable operation
ledger, and explicit run IDs. `AcceptanceTarget` describes each separate remote
account, workspace, and project/repository. Connection aliases must be unique
across all accounts, including connections not selected in the current run.
Never use a mutable global "current account" to resolve an OAuth provider.

Create at least two separate GitHub accounts and two Plane account/workspace/
project targets for full isolation acceptance. Supply the expected remote account
IDs and workspace identities independently of the returned MCP content.
`verifyIdentity` must compare both identity and workspace where applicable; a
successful response alone is insufficient. Each selection runs its read-only
identity probe before ordinary tools are exposed. GitHub's `get_me` and Plane's
`member(action="me")` are trusted local reads. Validate the actual response shape
in the embedding host; it can change independently of this driver.

### GitHub PAT

Store the PAT using `await secrets.set("github/account-a/pat", token)` with the
token injected from the environment or a secure UI. Build the connection config
with `githubConfig("github-account-a", "github/account-a/pat")`, set its expected
`accountId`, and use `{ owner: "disposable-owner", repo: "disposable-repository" }`
as its scope. The config contains secret references, never the PAT itself.

GitHub's official endpoint is `https://api.githubcopilot.com/mcp/`. Access tokens
use `Authorization: Bearer`. Tokens need access to the selected repository and
may require organization approval. See the [official host integration guide](https://github.com/github/github-mcp-server/blob/main/docs/host-integration.md).

### Registered OAuth

Create one `ManagedOAuthProvider` per immutable connection/account and share that
instance across its requests. Supply `ManagedOAuthOptions.clientInformation`
from the secure host, registered callback URI, explicit trusted issuer origins,
secure `SecretStore`, browser redirect handler, and remote revocation callback.
Use `oauthTransportFactory(alias => providers.get(alias))` as the driver's
`deps.transportFactory`. Route the registered callback URL to that exact
provider's `complete(callbackUrl)` and explicitly reconnect after completion.
The provider stores tokens, discovery, verifier, and state in account-scoped
secret entries. Do not put client secrets or callback URLs containing codes into
connection records, evidence, source files, or logs.

GitHub does not support dynamic client registration. Register a GitHub App or
OAuth App first and honor protected-resource discovery rather than hard-coding
authorization/token URLs. Organization installation/approval may also be needed.
Plane OAuth uses `https://mcp.plane.so/http/mcp` and binds authorization to the
workspace selected during sign-in. The host must verify that selection against
the target's expected workspace. Plane documents allowlisted client callbacks,
including localhost; confirm the actual registered callback works for this host.

Refresh acceptance requires an expired token or controlled expiry in a disposable
account, a shared provider instance, then an explicit reconnect and fresh
selection. Verify the same remote account/workspace again. Never replay a tool
operation to test refresh after its acknowledgement was lost.

### Plane PAT

Use `planeConfig("plane-account-a", "workspace-slug-a", "plane/account-a/pat")`
and set `workspaceId: "workspace-slug-a"`. Store the PAT at that injected secret
reference. Set `scope: { project_id: "disposable-project-uuid" }`. The hosted PAT
endpoint is `https://mcp.plane.so/http/api-key/mcp`; both `Authorization: Bearer`
and `x-workspace-slug` are required. The workspace header must match the immutable
connection workspace. Stdio connections require the same explicit binding via
`PLANE_WORKSPACE_SLUG`; `PLANE_API_KEY` remains secretized. See [Plane MCP documentation](https://developers.plane.so/dev-tools/mcp-server).

## Running a selection

Construct the driver with `execution: "live"` for real remote accounts or
`execution: "deterministic"` for controlled test transports. Implement
`evidence(event)` and `recordOperation(receipt)` separately. The ledger must
retain pending and terminal outcomes using the same operation ID. Treat exact
arguments and raw remote results as private application data.

Call `driver.select([connectionA, connectionB], contributionContext)` and retain
the returned run object. Only those targets are probed and discovered. Call
`run.call(connectionId, remoteToolName, arguments, optionalSignal)` with every
scope argument present and exactly equal to the selected target. The driver
rejects unselected accounts and mismatched repository/project arguments. A new
selection refreshes discovery and identity verification; an existing run keeps
its original connection revision and session.

All targets are read-only by default. Writes require both `disposable: true` on
the designated target and an `approveExactAction` callback. That callback must
show and approve the exact frozen, schema-validated invocation, including account,
workspace, revision, tool, and arguments, on every write. A permissive callback is
appropriate only in deterministic tests. Do not expose one in live setup or add
a generic "enable all writes" environment flag. Do not run external writes until
the owner has explicitly approved that exact action and disposable destination.

After a lost acknowledgement, expect `mcp_outcome_uncertain`, `retryable: false`,
and a terminal uncertain receipt. Reconcile the remote system using independent
reads before deciding what happened. Local operation IDs are not remote
idempotency keys. Terminal receipts preserve remote results and external IDs
when received. An unavailable receipt sink refuses dispatch; failure after a
remote response requires reconciliation.

## Acceptance sequence and evidence

For each authentication mode, perform the following with real credentials:

1. Select account/workspace/project A, verify identity, discover selected tools,
   and read one resource within its scope. Repeat with B. Confirm cross-scope
   arguments and calls to unselected targets are refused before dispatch.
2. With the default read-only configuration, attempt a known write and an unknown
   action. Confirm no approval or remote mutation occurs.
3. Only after exact owner approval, write once to the designated disposable
   destination. Retain the pending receipt, terminal receipt, and returned public
   external ID. Confirm denied and changed arguments do not dispatch.
4. Refresh OAuth credentials, reconnect explicitly, verify the same remote
   identity, and create a fresh selection. Confirm old staged tools/context do
   not follow a disconnected and reconnected session.
5. Edit the configuration revision, disable, disconnect, remove, and revoke in
   separate disposable scenarios. Confirm old tools refuse subsequent calls.
   Removal refuses locally before remote revocation; failed revocation does not
   restore local access. Remote revocation still needs independent confirmation.
6. Simulate a lost acknowledgement only against a dedicated disposable endpoint
   or controlled proxy. Confirm one dispatch, no replay, and an uncertain receipt.
7. Inspect durable connection configuration and captured logs for credential
   literals without copying those credentials into the evidence report.

Evidence has `schemaVersion: 1`, an explicit `execution` label, event kind,
nonsecret connection/account/workspace/revision identifiers, operation/run IDs,
tool/effect/outcome/error code, discovery count, and optionally public external
IDs. It deliberately omits raw arguments, response content, callback URLs, and
tokens. The trusted `externalIds` extractor must return public identifiers only;
never forward arbitrary remote strings or text. Evidence persistence failures
fail the operation path rather than silently claiming acceptance.

The deterministic suite covers two accounts/workspaces/projects, selected reads,
scope refusal, default write refusal, exact approval, external IDs, uncertain
outcomes without replay, and reconnect identity checks. Additional service tests
cover disabled/revised/removed connections, revocation failures, callback
immutability, pending receipt lifecycle, and selected-model media delivery.
Run:

```sh
bun test packages/mcp-client/tests/connections.test.ts packages/mcp-client/tests/connectors.test.ts examples/desktop-host/tests/managed-connector-acceptance.test.ts
npx --no-install tsc -p examples/desktop-host/tsconfig.json
```

These deterministic results are not live GitHub/Plane acceptance. Live PAT,
registered OAuth/callbacks, account/workspace verification, real expiry/refresh,
remote revocation, and approved writes remain unqualified until the host supplies
credentials, registrations, callback routing, expected identities, and designated
disposable destinations. No live write is run by this suite.
