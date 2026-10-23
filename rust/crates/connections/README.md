# Native connections

`agentkit-connections` provides native host APIs independent of REST routes.

`ConnectionRepository::for_app` fixes the application namespace for CRUD operations.
Updates and deletion require the current generation. Every update or recreation
gets a fresh generation; deletion does not reset the generation high-water mark.
`export` and `restore` provide validated, versioned metadata snapshots. The host
must persist snapshots atomically and protect them from rollback. Credentials and
consent tokens are never included. Storage is in memory until the host persists it.

`NativeConnections::plan` captures an immutable connection, destination, explicit
model, capability, disclosed categories, and exact payload. `approve` consents to
that exact plan. `approve_scope` explicitly permits later payloads only while the
application, connection generation, full connection configuration, destination,
account, capability, model, and disclosed categories remain identical. Scope
consent relies on the host accurately declaring all outgoing data categories.
No account, model, or destination fallback occurs. `plan_to` explicitly selects a
route within the configured origin and base path; route changes need new consent.
Remote plaintext HTTP is rejected unless the host explicitly sets
`allow_insecure_http`; loopback HTTP remains available for local providers.

Execution checks consent and connection state, then checks the network adapter's
availability before reading credentials. It checks again after the credential
read and after dispatch. Revocation or a changed connection discards late results.
A request already dispatched cannot be unsent. Hosts must coordinate cancellation
with their transport if they need to stop an in-flight request immediately.

`execute` uses the injected `ConnectionNetwork` port and returns a buffered result.
`execute_with` preserves the transport's native response type, including streaming
responses. Stream consumers must call `RequestFence::check` before publishing
each event. The injected transport must use the exact approved destination and
payload, reject redirects, enforce request bounds/timeouts, and avoid logging
credentials or user content. Model discovery and probing use explicit capabilities
through the same consent path. The optional `reqwest-transport` feature supplies an
OpenAI-compatible transport with redirect rejection, a request timeout, bounded
buffered responses, and an unbuffered `send_streaming` method. Other protocols must
provide their own adapters; an unavailable provider kind fails before secret reads.

`SecretStore` is asynchronous and injectable. Available adapters are:

- `MemorySecretStore`: explicit ephemeral storage, with zeroization on drop.
- `EnvironmentSecretStore`: explicit application/connection/generation/account
  bindings to environment variable names; read-only, with no implicit fallback.
- `UnavailableSecretStore`: fails closed.
- `KeychainSecretStore`, behind `os-keychain`: the platform keyring backend, with
  all blocking calls on Tokio's blocking pool. Failed or unavailable keychain
  operations do not fall back to plaintext files, memory, or the environment.

Credential keys include the application, connection, account, generation, and
purpose. Header names are immutable connection metadata; header values are a
separate secret-store entry. `provision_headers` validates exact names and keeps
values out of projections, snapshots, and debug output. Built-in authorization,
content-type, accept, host, and framing headers cannot be overridden.
Updating a connection requires explicitly provisioning the credential for its new
generation. The host should delete obsolete credential keys when no longer needed.
Hosts must advance the connection generation before rotating credentials or header
values. They may explicitly copy secrets to a new generation only when preserving
the same account. `execute_resolved_with` supplies both credentials and headers to
streaming transports; credential-only `execute_with` fails if headers are configured.
`Secret` intentionally implements neither `Debug` nor serialization; expose it
only when constructing the approved transport request.

Tests cover isolation, generation fencing, consent revocation, exact and reusable
consent bindings, adapter preflight, late asynchronous results, snapshot validation,
and streaming result fencing. The `os-keychain` feature is compiled in validation;
mock tests do not qualify real OS keychain behavior. A real macOS keychain access,
lock/unlock, cancellation, restart, and permission-prompt audit remains a host
integration responsibility.

## Opt-in real keychain qualification

Run the synthetic probe explicitly:

```sh
cargo +1.97.0 run --manifest-path rust/Cargo.toml -p agentkit-connections \
  --features os-keychain --example keychain_qualification
```

The program prints a fresh UUID service, two synthetic application namespaces,
and one synthetic connection. It uses only the public `KeychainSecretStore`
adapter, checks missing entries before writing, and verifies put/read/delete,
missing-after-delete, and cross-application isolation. It never prints credential
values, reads a provider credential, sends a provider request, or uses a fallback.

Cleanup runs for every key whose write was attempted, including after a read
failure. Each OS operation has a five-second deadline. Keychain calls cannot be
cancelled once dispatched: after a timeout, the probe reports failure and cannot
guarantee final cleanup. The printed service identifies the synthetic namespace
for a subsequent manual inspection. Do not bypass permission prompts or change
keychain security settings to make this probe pass.

A successful run qualifies only the synthetic operations in that OS session.
Lock/unlock, restart, signed application access control, permission-prompt
interaction, and all other platform backends require separate qualification.

The local run on 2026-10-22, macOS 26.6.2 (25G83), exited with status 1.
Both initial missing checks passed; the first synthetic write returned
`SecretUnavailable`. Cleanup's delete call succeeded and its subsequent read
verified the attempted application's entry missing. The second application's
write was never attempted.
No timeout occurred, security settings were unchanged, and no prompt was bypassed.
The service was
`AgentKit.synthetic-keychain-qualification.855d191a-bbff-4916-934a-58f31b97a917`.
This is a failed real-platform probe, not successful CRUD or isolation qualification.

HTTP provider metadata changes use a shared authority reservation. Pending changes
reject reads, planning, execution, and other writers before secret access. One
SQLite transaction guards the prior app snapshot and writes connection metadata,
selected model, and catalog changes. Acknowledged publication updates the original
shared authority; a known rollback leaves it unchanged. Uncertain commit outcomes
or cancellation before publication poison that authority until it is reopened
from durable storage. Counters stop at JavaScript's maximum safe integer.

Secret provisioning, generation-to-generation copying, and old-slot retirement
run after durable metadata publication while the writer reservation remains held.
A secret failure keeps the committed generation and returns `secret_write_failed`;
retry PATCH with the intended `apiKey` and `extraHeaders`. Execution never falls
back to an older generation's slots. Cancellation or secret-store failure can leave
up to two obsolete purpose slots per affected generation; repeated failures can
accumulate slots. The generic SecretStore API cannot enumerate them. Hosts must
retain references for explicit cleanup when their store needs that guarantee.
No rollback or automatic account switch recovers a missing current credential.
