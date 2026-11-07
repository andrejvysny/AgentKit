# Native profile and selected-content admission

Native session admission resolves a profile before it creates any durable row.
`ProfileRegistry::default()` supplies `chat`, `coding`, `coding/plan`,
`coding/supervisor`, `coding/reviewer`, `research`, and `automation` profiles.
Unknown profiles are errors. Overrides passed to `ProfileRegistry::new` inherit
the built-in record of the same id; a new profile can use `extends` to name
another profile. Cycles, unknown parents,
unsupported permission modes, duplicate tools, and invalid limits are rejected.

The resolved profile and its SHA-256 digest are stored in the session descriptor.
Every admitted turn carries `profileSnapshot` and `profileDigest`. Later registry
changes cannot widen that session. An explicitly supplied profile digest must
match the resolved profile. Reopen the existing session to retain its snapshot;
create a new session to use a changed profile. A legacy session without a
snapshot resolves its named profile on its next new admission and rejects an
unknown name rather than bypassing policy. Already queued session tasks without
immutable profile authority fail with `profile_snapshot_required`; they are never
silently restamped from a mutable registry. Submit session turns through
`AgentSessionService`; `AgentHost::submit_chat` is for ordinary chats and refuses
session chats before writes with `session_submission_required`.

The profile tool allowlist filters the final staged registry, including tools
from contributors. An empty allowlist stages no tools. Role and live permission
guards remain in force. Explicit create-session model/reasoning settings override profile defaults; provider defaults apply last.
explicit HTTP turn settings override session defaults. The initial permission
mode comes from the profile unless the host explicitly supplies one. Limits are
the minimum of host, profile, and explicit task bounds. Subagent role limits
cannot raise a profile or host ceiling.

## Transcript and context limits

Native built-in profiles preserve the full eligible transcript. They deliberately
omit the historical TypeScript count limits. `ChatExecutorOptions.history_limit`
defaults to `usize::MAX`. An explicit host/profile history limit is an admission
ceiling: an oversized transcript fails with `context_overflow`; it is never
silently sliced.

Context admission applies to session and ordinary host turns. Token counts are
UTF-16 character estimates (rounded up at four characters per token, plus a fixed
image allowance), not model-exact tokenizer counts. A positive safe
integer `task.payload.contextWindowTokens` freezes the host's model window and
has precedence over the session/catalog fallback. `maxOutputTokens` is also a
positive safe integer and may only lower host/profile output ceilings. Without
a task window, the native host checks the session's explicit window, then matching frozen native connection/model metadata, then stored provider model metadata, then its documented fallback. Native model context metadata is read from the admitted connection snapshot, so later catalog changes cannot widen that bound. Requests that do not fit
fail with `context_overflow` before provider dispatch.

`windowSource` retains the shared event vocabulary: `descriptor` includes a frozen per-turn window override as well as a session descriptor; `model` includes frozen native model metadata and the stored model catalog. These labels do not change admission precedence.

Automatic compaction requires `ChatExecutorOptions.auto_compaction = true`.
An explicit session compact request remains supported. Tool-result elision is disabled by default. Supply an explicit
`context.toolResultElision` options object to enable it, or `false` to preserve
the complete tool transcript explicitly.

## Selected managed content

`freeze_run_content(store, selection, chat_id, authority)` resolves published
prompt templates and pinned managed skills into a durable snapshot. Selection is
`{template?: {id, revision, arguments}, skills?: [{id, revision}]}`. A template-only
selection is valid. The snapshot contains the selected revisions, resolved text,
consumed host-authorized connector identities, their authority scope, and a digest. Admission stores it under
`contentSnapshot` alongside `contentSelection`. Execution rejects a selection
without its snapshot, a changed snapshot digest, or a revoked connector grant.
Published content may change or disappear after admission; the admitted turn
still uses its frozen text.

`ContentAuthority` supplies current authorized connector ids for a chat. Without
that host port there are no connector grants. A request body cannot grant itself
connector access. Hosts with account-scoped grants override `ContentAuthority::authority_identity`
to return exact selected session, account, connection and generation identities.
The snapshot binds that value, and a replacement grant fails with
`content_authority_changed` even when the connector name stays the same.
The host rechecks the frozen grants before every provider
request, including later tool iterations and resumed attempts.

The executor places resolved instructions in `ChatRequest.selected_content`,
separate from durable `messages`. Both native provider implementations transmit
that content and typed `ReasoningEffort`. The Responses checkpoint keeps selected
items out of canonical transcript counts, digests, and replay items. Every new checkpoint emits a contract `runContent` binding, including empty selections, which pins the admitted run id and content digest. Legacy unbound checkpoints remain readable but are rebuilt from canonical history when selection context changes. The aggregate checkpoint envelope remains limited to 1 MiB. A
same-run digest change fails closed. A legitimate change on a later turn rebuilds
the request from complete canonical history and the new selection, discarding
private state tied to the previous selection.

## Binding migration

`NativeServiceOptions.profiles` accepts partial overrides and `extends`.
`NativeSessionService.setContentAuthority(authority, identity?)` supplies connector
grants and optional exact account authority through the
existing fenced callback bridge. JS provider callbacks receive `reasoningEffort`
and standard `runContent: {messages}`; callback adapters must forward those fields
to their provider. Rust provider lanes remain incremental; JS callbacks retain
their documented final-result semantics.

Native transports recheck the task and selected-content guard after consent and credential resolution, immediately before the HTTP send. Custom Rust transports must override `TrustedTransport::request_guarded` and check the supplied guard after their own awaited preparation. JS callback adapters receive `request.checkAuthority()` and must await it after their own preparation, immediately before sending. The bridge rejects checks after cancellation or settlement. An arbitrary custom adapter that ignores this port does not gain native wire-last fencing.

`listNativeProfiles(overrides)` and `resolveNativeProfile(id, overrides)` query the same Rust registry without opening a store or provider. Pass the same overrides to `createService`; do not duplicate the TypeScript registry as native admission authority. `createSession` accepts typed `reasoningEffort` and `contentSelection` directly.

Hosts that construct raw `SubmitTask` values should call `freeze_run_content`
before submitting selected content, and place the returned value in
`contentSnapshot`. Applications that use `AgentSessionService` or native HTTP
admission receive frozen profile/skill snapshots automatically. No code tool,
browser OAuth interaction, or external server is enabled by choosing a profile.

Regression witnesses live in `host/tests/native_parity.rs`,
`host/tests/http.rs`, and `core/src/providers/responses_request.rs`.

## Explicit provider ports

The binding's HTTP composition can select `providerSecrets: "callback"` and
`providerOperations: "native"`. `setProviderSecretStore` supplies the application's
own durable vault; only secret references persist in provider configuration.
There is no implicit volatile fallback. `providerSecrets: "memory"` remains an
explicit process-local option. An OS keychain implementation is not qualified
by these ports.

Native session admission uses `ChatProviderFactory::snapshot` to freeze the
selected connection before it creates durable turn rows. Unknown, deleted, or
revoked native providers fail admission; only explicitly registered static
provider ids may use the callback/static lane.

Native management operations and execution share one restored connection
repository, vault, and exact-consent authority. The service retains those
identities across HTTP stop/start. Replacing a vault or consent handle requires
a new explicit composition and is rejected by a bound factory. Binding callback vault and consent handles are one-shot per service; reinstalling the identical handle is allowed, replacement is refused. Passing `null` clears future callback access only: it does not revoke already-approved requests or abort a callback already executing. Revoke live provider authority through the native connection lifecycle. Each native
request obtains consent for the exact account, destination, model, serialized
payload and disclosed data categories; the callback receives an AbortSignal.
Discovery and probe operations have a bounded deadline. The default JS provider
callback remains available when native provider operations are not selected.


## Native MCP host ports

The native client exposes explicit OAuth composition through `connectMcpWithOAuth`, `completeMcpOAuth` and `revokeMcpOAuth`. The host supplies an application namespace, safe generation, immutable connection/account/resource/redirect/origin binding, durable secret callbacks and a browser redirect callback. Native OAuth enforces PKCE, state TTL of 10 minutes, issuer and replay checks; credential material, client secrets and verifiers stay behind the secret port. Refresh is single-flight across handles sharing the bound key. Revocation persists a tombstone with generation-isolated keys. Authorization opens only through the explicit redirect callback; native code does not approve consent or replay a failed resource operation. Request authority is checked after refresh immediately before resource dispatch.

`connectMcpWithCallbacks` and the OAuth helper accept an elicitation handler. Elicitation capability is advertised only when installed. Accepted content is schema-validated. Cancellation, disconnect and store close abort the active callback signal; stale helper disconnect uses its immutable handle and cannot remove a replacement connection. Native lifecycle fences survive store reopen and fence pending connects as well as connected tools.

Rich MCP model projection marks sources as untrusted. Images/audio require
explicit host capability support, while the full UI content remains available.
Sampling is unsupported. Optional `httpCallback` is an explicit trusted buffered
HTTP host/test port for OAuth discovery/token requests and MCP resource POST
requests, with no redirect following. Resource GET/SSE and DELETE requests still
use native Rust HTTP; default native HTTP/SSE remains incremental.

The HTTP MCP configuration repository is composed with the same Store manager. It persists secret references through a configured vault, redacts public DTOs, revokes changed aliases before commit, and reports live metadata only for the matching durable generation. Configure the one-shot vault port with `mcpConfigureSecrets` before storing literals; absent vault authority refuses them. Unrelated manually connected aliases remain separate. Alias preflight runs before durable/live authority changes. Cancellation releases the unpublished reservation; already-invalidated aliases remain fenced after an ambiguous/cancelled commit until explicit higher-generation reconciliation or initialization. Old handles are never restored. Replacing the vault requires reopening the store. Store close cancels pending network and callback work.

Native regression witnesses include `mcp-hardening.test.ts`, `mcp-config-runtime.test.ts`, `mcp/tests/oauth*.rs`, `mcp/tests/elicitation.rs` and `host/tests/mcp_configuration_ports.rs`. These loopback/stdio tests do not establish live third-party OAuth, cross-platform abrupt-process behavior or remote HTTP notification replay qualification. Generic managed-content connection actions remain explicitly unsupported; they are not aliases for these MCP APIs.
