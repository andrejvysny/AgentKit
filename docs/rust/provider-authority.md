# Provider connection authority

Durable native provider identity is scoped to a database namespace, a caller's
non-secret logical key, the exact endpoint string, and a trusted account scope.
Two independently created stores never share an identity merely because their
logical keys match. Copying a database intentionally preserves its namespace;
a database copy is not a new independent store.

`NativeSessionService.registerProviderConnection(baseUrl, apiKey, options)`
accepts `connectionKey`, `storePath`, and optional `accountScope`.
The host supplies the account scope from trusted account configuration; it
must not use an API key, a credential digest, or untrusted model output.
An omitted account scope permits ordinary registration but cannot authorize
an explicit continuity transfer.

An unchanged endpoint, account scope, and credential restores the same identity
and generation after restart. Credential rotation advances the generation.
Revocation persists across restart; re-registration after revocation advances the
generation even when the credential is unchanged. Changing the endpoint or
account scope creates a different identity and retires the previous registration
for that logical key. Returning to a retired endpoint cannot revive its old
generation.

## Continuity after rotation

Rotation never rewrites admitted run bindings or continuation state. Stale
bindings and late continuation saves are refused rather than normalized.
The embedding host can explicitly transfer compatible settled history by
registering with:

```ts
await NativeSessionService.registerProviderConnection(baseUrl, rotatedKey, {
  connectionKey: "primary",
  storePath,
  accountScope: trustedAccountId,
  restoreContinuity: {
    expectedGeneration: 2,
    previousGeneration: 1,
    confirmUnchangedScope: true,
  },
});
```

The confirmation attests that the account and endpoint are unchanged. The
expected generation is a compare-and-set condition on the resulting registration.
Registration and restoration commit in one transaction; a failed condition
cannot partially rotate credentials. Existing local transports may be fenced
conservatively while an attempted rotation is checked.

Only tasks in `completed`, `failed`, or `cancelled` state are eligible.
Active, queued, interrupted, and approval-waiting invocations retain their original
generation. Transfer preserves private input items and requires the stored
continuation scope to match its run binding. No active invocation is restamped.

## Schema migration

Rust schema 13 adds a durable random namespace and endpoint/account/revocation
metadata. Migration is additive and transactional. It preserves legacy
connection rows and all history; legacy endpoint/account fields remain unknown,
so those rows cannot authorize dispatch or continuation saves.

After verifying the historical endpoint and account, the embedding host may
register the same logical key with `restoreContinuity.legacy: true`, the legacy
`previousGeneration`, the new `expectedGeneration`, and
`confirmUnchangedScope: true`. This explicitly transfers compatible settled
legacy scopes into the new scoped identity. The legacy row remains preserved and
blocked. Unknown historical scope is never inferred from its credential digest.

## Dispatch, delivery, and storage

Local rotation or revocation invalidates existing transports through an atomic
liveness fence. Durable authority is checked off-thread before HTTP dispatch
and while delivering provider events. Cross-handle changes therefore invalidate
delayed responses as well. Continuation admission and saves validate authority
inside the same SQLite transaction that writes the binding or state.

A stale request rejected before dispatch sends no HTTP. A request already sent
may complete remotely, but stale events and continuation state are refused.
Revocation does not promise to undo remote work or erase historical data.

Focused regression coverage lives in `provider_authority_tests.rs`,
`provider_connections_tests.rs`, store continuation tests, and the native
multi-process `provider-continuity.test.ts`. Loopback tests qualify authority
behavior; they do not establish live-provider or cross-platform release readiness.
