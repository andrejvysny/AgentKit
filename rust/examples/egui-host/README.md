# Native egui reference host

This example consumes the native Rust SDK directly. It does not serve an HTTP
loopback API or wrap the TypeScript runtime. The GUI exchanges bounded commands
and asynchronous updates with a Tokio runtime; SQLite, provider work, and consent
waits run outside the UI thread.

Run the explicit offline demo:

```sh
cargo run --manifest-path rust/Cargo.toml -p agentkit-egui-host --locked
```

The demo provider streams a deterministic echo in process. It uses the real
`NativeConnections` plan, exact approval, and generation fence, but its transport
performs no network I/O and reads no credentials. The URL shown in the demo
consent dialog identifies a scope only. `demo-slow` makes cancellation visible.

The application uses `AgentHost`, `ChatExecutor`, `SqliteStore`, durable event
subscriptions, and real proposal transitions and receipts. A proposal replaces a
local demo note in SQLite. Approval is separate from apply; replaying an applied
proposal reads its receipt. This is a deliberately idempotent local setting
replacement, not a general external side-effect or filesystem applier.

## Persistence and recovery

The default database is `agentkit-egui-demo.sqlite3` in the working directory.
Choose a stable path explicitly when needed:

```sh
AGENTKIT_DEMO_DB=/absolute/path/reference.sqlite3 \
  cargo run --manifest-path rust/Cargo.toml -p agentkit-egui-host --locked
```

Closing the application stops its runtime; persisted work and events remain.
The demo lease lasts 3 s. Reopen the same database after expiry and choose **Scan
expired runs**. Manual recovery leaves interrupted work paused. Choose **Resume**
on the intended run; resumption requires fresh request approval. There is no
automatic provider, model, account, or consent fallback.

## Optional live provider

Configure an OpenAI-compatible API base URL and an exact model before launch:

```sh
AGENTKIT_PROVIDER_URL=http://127.0.0.1:1234/v1 \
AGENTKIT_PROVIDER_MODEL=local-model \
  cargo run --manifest-path rust/Cargo.toml -p agentkit-egui-host --locked
```

For a credentialed endpoint, set `AGENTKIT_PROVIDER_KEY_ENV` to the name of an
existing environment variable holding the key. The application binds that exact
variable through `EnvironmentSecretStore`; it does not read its value until
request approval. Never place the secret value in a URL or command argument.
HTTPS is required for remote endpoints. `AGENTKIT_ALLOW_INSECURE_HTTP=1` explicitly
permits a remote HTTP endpoint; use it only when you intend to send plaintext.
Local loopback HTTP requires no opt-in.

Every live provider request shows the exact app, connection, generation, account,
destination, model, and disclosed data categories. **Approve once** authorizes
only that immutable payload. **Deny** fails the request. Requests use the host's
`NativeConnectionTransport`, reject redirects, and check connection fences during
stream consumption. This example does not implement account OAuth, OS-keychain
setup, model discovery, custom secret headers, or a provider settings editor.

## Validation scope

The headless backend test covers chat approval and denial, completed and failed
task persistence, proposal approval, apply, and receipt replay. The native binary
is compiled and linted separately. Live providers, credentials, OS-keychain
prompts, and desktop automation require environment-specific qualification; a
successful offline test does not establish those capabilities.

## Local verification and stream phases

After a completed run, the reference host checks that a nonempty final assistant
answer is durably stored. If a demo note was applied, it also checks its receipt
and saved value. Reports are durable `run.verification` events, shown in the
**Local verification** panel and reloaded on refresh/reopen. A missing answer or
changed note produces a partial report. These checks establish local persistence
invariants, not the factual accuracy of model output or external domain effects.

The UI consumes `agentkit_client::RunState` for sequence validation, replay
deduplication, retry-pass text resets and presentation phases. A provider terminal
event leaves a run settling; streamed text remains until the durable task snapshot
reports settlement.
