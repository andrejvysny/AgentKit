# Managed connectors, skills and prompt templates

The TypeScript implementation extends the existing host and MCP adapter. It does
not introduce another agent loop. `examples/desktop-host/src/managed-integrations.ts`
provides a typed composition recipe for an embedding application.

## Invocation and compatibility changes

`createMcpToolSetContributor` now classifies unknown actions conservatively and
refuses external writes without an `authorize` callback. A host can supply a
trusted, argument-aware `classify` callback for known reads. Remote
`readOnlyHint` annotations remain descriptive. Every authorization sees an
immutable snapshot of validated arguments. Native `ToolGuard.canExecute` also
receives arguments, run identity and cancellation.

A dispatched `tools/call` that loses its acknowledgement returns
`mcp_outcome_uncertain` with `retryable: false`. Timeouts and cancellation after
dispatch also require reconciliation. `retryTimeouts` cannot override this rule.
Only a trusted low-level caller explicitly passing `replaySafe: true` can opt a
known replay-safe operation into recovery. A local operation ID does not provide
remote idempotency.

Discovery follows cursors under page, item and byte limits, rejects repeated
cursors, preserves case-sensitive remote names, and bounds provider names to
64 characters. Core validation preserves `$schema` and `$id`, supports draft-07,
2019-09 and 2020-12, and rejects unknown dialects, validation keywords and formats.

The adapters use the official `@modelcontextprotocol/client`, `server`, and
`core` packages at **2.4.0**, verified against the npm registry. SDK automatic
negotiation supports **2026-07-28** and the older initialize handshake. The
inbound HTTP tests exercise every revision in `MCP_COMPATIBILITY.verified`;
outbound HTTP and stdio tests exercise 2026-07-28 and 2025-11-25 peers over real
loopback sockets and child processes. Node 22 and Bun 1.4 are locally qualified;
other runtime versions remain CI gates. Supported protocol constants alone are
not evidence that every transport/revision pair has been exercised.

Both inbound paths use application-generated actors independent of transport
session identifiers and apply the same permission checks. Modern input-required
interactions use the host elicitation callback; capability advertisement and
cancellation are tested. The compatibility declaration separates supported
revisions from the transport matrix actually exercised.

Inbound cancellation now combines request and actor lifetimes. Cancelled queued
calls do not execute. Resource and prompt callbacks receive request cancellation.
An unserializable successful result is reported as `isError: true`.

## Connections and credentials

`ConnectionService` owns create/update, connect, test, reconnect, disconnect,
remove, reconciliation and cleanup. `catalog()` exposes the connector definitions.
A connection has an immutable connector, account and workspace identity plus a
configuration revision. Create separate connections for separate accounts.

`MemoryConnectionStore` and `SharedSqliteConnectionStore` implement the persistence
port. The SQLite store accepts a caller-owned AgentKit database handle, creates
its optional table and shares its write gate. Stores use compare-and-swap
revisions. `reconcile()` applies out-of-band durable edits to live sessions.
Applications call it when another process updates configuration.

A run's selection callback returns explicit `ConnectionGrant` records. Selection
precedes bounded parallel discovery. Each grant pins connection ID, account,
workspace and revision, names allowed tools/effects, and can constrain exact
argument values such as GitHub owner/repository or Plane project. An existing
run refuses changed, disabled, disconnected or removed connections. A staged
run also pins the live connection instance, so disconnect/reconnect cannot restore
an old grant even when its durable configuration revision is unchanged. Account
aliases must be unique across persisted connections.

Every write requires `authorize(invocation)`. The host renders and approves that
exact immutable invocation. The operation ledger receives a pending receipt
before dispatch and a terminal outcome with the external result after dispatch.
`recordOperation` should upsert by operation ID or retain its lifecycle events.
Receipt storage failure after a remote response is reported explicitly as an
uncertain outcome; callers must reconcile before retrying.

Header and environment literals are normalized into `SecretStore` references
before managed configuration is persisted. Existing MCP REST CRUD does the same
when `deps.secrets` is wired, and refuses literal credentials otherwise. Redacted
edits preserve values only for an unchanged transport target.
`ManagedMcpConfigStore` decorates the existing config store when an application
wants its existing `/v1/mcp/servers` CRUD to update a live manager.

`ManagedOAuthProvider` uses the official SDK authentication entry point for
protected-resource and authorization-server discovery, PKCE and token exchange.
It validates callback URI, single-use expiring state, trusted HTTPS issuer
origins and exact resource identity. Client information, verifier, state,
discovery and tokens are account-scoped in `SecretStore`. Refresh is coordinated
within each provider instance; applications must share one provider per account.
A distributed host must provide cross-process credential coordination. Browser
opening, registered client information, callback routing, identity verification
and remote revocation are application callbacks. Stored OAuth entries use a
versioned binding to the canonical resource, configured client/issuer, exact
callback, client metadata, and trusted origins. A resource or identity edit cannot
reuse an unexpired token. Legacy unbound or mismatched material requires explicit
`invalidateCredentials("all")` and fresh authorization; reconnect alone does not
migrate it. Disconnect removes mismatched local material without sending it to a
new revocation target. Current-binding revocation failures remain visible after
local cleanup. HTTP authorization failures are not replayed as tool operations.

The supplied reference definitions are:

- GitHub: `https://api.githubcopilot.com/mcp/`, token or pre-registered OAuth
  client. No assumption of dynamic client registration.
- Plane: `https://mcp.plane.so/http/mcp` for OAuth,
  `https://mcp.plane.so/http/api-key/mcp` for PAT, or explicit stdio configuration.
  PAT workspace headers and stdio workspace variables must match the connection
  workspace. Duplicate case-insensitive headers are refused. Resource tools are
  classified using their action arguments. Unknown actions require approval.
- Custom MCP: HTTP/stdio, with conservative classification by default.

## Remote context and rich results

The manager exposes `listResources`, `readResource`, `listResourceTemplates`,
`listPrompts` and `getPrompt`. Catalog operations are bounded and paginated.
Resource and prompt results retain origin and `trusted: false`; remote prompt
roles and content are preserved. Catalog-change callbacks let applications
invalidate UI caches. The adapter does not cache remote catalogs between runs.
Optional elicitation callbacks are advertised only when supplied by the host.

Tool results attribute resource links and embedded resources as sources.
`modelContentCapabilities` explicitly enables image or audio delivery for the
selected model. Managed hosts forward these capabilities per run through the
connection service; omitted capabilities remain disabled. Attachments stay in the persisted tool envelope and are delivered
as attributed user content after balanced tool results, including on replay.
They share the serialized result byte budget. OpenAI-compatible chat adapters
map images and WAV/MP3 input audio; the Responses adapter continues to reject
unsupported audio explicitly. Unsupported modalities remain available in raw UI
data and produce warnings.

## Templates and skills

`PromptTemplateService` publishes immutable `(id, revision)` templates, validates
arguments and renders `{{variable}}` placeholders without evaluation. Messages
retain their system/user/assistant roles. Missing and unknown arguments fail
before execution. Memory and shared SQLite stores, render/preview APIs, and the
inbound `templatePromptSource` adapter are supplied. The inbound adapter exposes
only selected versions with user/assistant roles. Deleted versions retain a
tombstone: the same `(id, revision)` cannot be republished with different content.
Optional managed-content tables are validated when reopening the shared SQLite
database; malformed or unknown tables still fail closed.

`SkillService` catalogs explicitly configured application bundles or directory
sources, parses safe scalar/block `SKILL.md` frontmatter, activates explicitly
selected instruction hashes, and progressively reads bounded references. Complex
YAML tags, anchors and collections are refused. `fileSkillSource` from `@agentkit/runner-local/filesystem`
checks real paths, symlink containment, file type, byte limits and cancellation.
It opens without following a final symlink, verifies file identity before and
after bounded reads, and detects parent substitution before reading. Portable
Node filesystem APIs do not provide atomic ancestor resolution against a hostile
process continuously renaming directories; such deployments need an OS sandbox.
Default package imports do not load filesystem services; the umbrella package
exposes them through `agentkit/runner-local/filesystem`.
Supporting-resource reads include a content hash and an optional audit callback.
`metadata.requires-connectors` can declare comma-separated dependencies on
already selected authorized connectors. A declaration never creates a grant.
There is no automatic installer or script executor.

`SubmitMessageInput.contentSelection` (also exposed in REST submit/regenerate)
pins template revisions and skill hashes in the durable task. `runContent`
resolves those selections before provider creation or connector discovery.
Responses continuation stores pin the selected-content digest for the current
run without adding transient instructions to canonical conversation history.
Selected content is delivered once across tool passes and durable replay; a later
turn can choose different content or none without losing the prior Responses
reasoning chain. A changed skill hash fails
rather than silently replacing instructions. `ContextProvider.promptBlocks` is
used when there is no custom `systemPrompt`; hosts with a custom composer keep
legacy behavior unless they select `promptBlockMode: "append"`.

## HTTP surface

Optional services return 501 when not configured. Connections use the privileged
`mcp_config` authorization resource; templates use `prompt_template`, and skill
catalogs use `skill`.

| Operation | Route |
| --- | --- |
| List connections | `GET /v1/connections` |
| Save a revision | `PATCH /v1/connections/:connectionId` |
| Remove | `DELETE /v1/connections/:connectionId` |
| Connect/reconnect/test/disconnect | `POST /v1/connections/:connectionId/actions` |
| List/publish templates | `GET/POST /v1/prompt-templates` |
| Remove template version | `DELETE /v1/prompt-templates/:templateId/:revision` |
| Render version | `POST /v1/prompt-templates/:templateId/:revision/render` |
| Skill metadata | `GET /v1/skills` |

The typed client follows the shared route table. Applications still own account
selection, settings and approval presentation. The reusable
[acceptance harness](../examples/desktop-host/src/managed-connector-acceptance.ts)
uses the same production connection service. Follow the
[live acceptance instructions](managed-connector-acceptance.md) for trusted
identity probes, disposable targets, exact-action approval, and evidence capture.
Deterministic tests do not qualify live GitHub/Plane OAuth, account identity,
revocation, or external writes. Those checks still require application credentials,
registered OAuth callbacks, and explicitly designated disposable resources.

## Rust boundary

The separate native runtime keeps its own SQLite database identity; it must not
open the TypeScript database. Shared schemas, routes, and conformance fixtures
are regenerated from their authoritative sources. Strict unknown-keyword and
format checks and nested resource IDs now use the same expected outcomes in the
shared corpus; there is no `expectedTypeScript` override for these cases.

Native optional connection, OAuth, template, and skill services remain
unimplemented. Their routes return authenticated `501` responses. Selected
content on submit/regenerate is rejected before creating tasks, messages, or
provider work, rather than silently ignored. Shared rich-result DTOs do not
qualify the native rich-content delivery pipeline.

The pinned native JSON Schema evaluator has an evaluated-property limitation:
modern schemas combining `unevaluatedProperties` and complex `patternProperties`
are refused. Supported literal patterns and the exact restriction are documented
in [native parity](rust/parity.md). This is an explicit bounded limitation, not
full modern schema parity. See [Rust validation](../rust/README.md) for the pinned
toolchain, native tests, wasm target, and TypeScript-to-native conformance gates.
