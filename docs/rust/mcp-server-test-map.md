# Phase 1 gate: `packages/mcp-server` TS tests → Rust/native coverage map

112 cases across 11 files (+helpers fixtures). Verdicts: ✅ covered (read-and-verified same-behavior assertion) · ⚠️ partial · ❌ GAP (port) · 🗑️ documented-drop (native lane never had it; zero consumers).

Verification basis (covering tests read): `packages/native/tests/mcp.test.ts` S1 :214 (init, gated catalogue, round-trip, bearer refusal, session reuse), S2 :321 (writesEnabled, validation, close live); `packages/native/tests/tool-identity.test.ts` :119/:196; `rust/crates/mcp/tests/security.rs` SEC1 auth-precedes-catalogue/session-borrow, SEC2 rebinding/body-bounds/eviction, SEC3 protocol-freeze vs installed SDK1; `rust/crates/mcp/tests/native.rs` N1 lists+calls+actor-revocation, N2 extensions reject guesses/bad args, N3 deadline cancels backend scope, N4 five revisions + newer refused; `inbound_dispatch.rs` IB catalogue-cancellation zero-dispatch; `managed_http.rs` MH rotation reauth+revoke; `json_boundary.rs` JB bijection; `napi/src/mcp_server.rs` NP parse/limits.

Structural absences in native lane (`napi/src/mcp_server.rs` "documented reduction", `server.rs:301`): sessionScope header hook, MCP-lane ToolGuard chain (`dispatch_guard: None`) + bindings (`Vec::new()`), custom `verify()`, lenient bearer parsing, stateless per-request mode. `guards.rs` had ZERO unit tests.

## handler.test.ts (37) — 17✅ 9⚠️ 11❌
| case | covering | verdict |
|---|---|---|
| tools/list verbatim, no namespace | S1, N1 | ✅ |
| hides write tools; call path filters | S1, N1, tool-identity:196 | ✅ |
| writesEnabled lists+calls writes | S2 | ✅ |
| unknown tool → protocol error, session survives | S1, N1 | ✅ |
| two-block summary+modelData projection | S1 pins native shape (JSON text + structuredContent) | 🗑️ shape superseded |
| failed tool isError + structured data | — | ⚠️ port ok:false wire |
| bad args validation isError | N1 + napi validator | ✅ (TS errorCode fields 🗑️) |
| missing bearer → bodyless 401 + www-authenticate | SEC1 401 | ⚠️ port header assert |
| wrong token 401 | S1 | ✅ |
| wrong same-length token 401 (no length leak) | S1 32-vs-32 + ct_eq | ✅ |
| auth before session lookup | SEC1 | ✅ |
| disallowed Host → bodyless 403 | SEC2 | ✅ |
| loopback any port default | S1/S2 | ✅ |
| Origin refused once configured | SEC2 + SEC1 | ✅ |
| custom allowedHosts with ports | — | ❌ guards unit |
| per-client sessions + scoped tools | S1; scoped-header 🗑️ | ⚠️/🗑️ |
| scope pinned at init | SEC1 404 + MH | ✅ |
| unknown session 404; no-session POST/GET 400 | SEC1/SEC2 404; 400 untested | ⚠️ port 400 |
| DELETE forgets session | — | ❌ port |
| dispose closes live, 503, idempotent | S1 stop + SEC1 | ✅ |
| PUT/PATCH/OPTIONS 401→405; version-after-auth | — | ❌ port |
| actor revoked exactly once (evict + shutdown) | SEC1+SEC2 | ⚠️ port exact-once |
| binding owner keeps session | SEC1 | ✅ |
| other principal → 404 indistinguishable | SEC1 | ✅ (body-identity ⚠️ minor) |
| DELETE by other → 404, session alive | SEC1 | ⚠️ port alive-after |
| unauth custom-verify fingerprint | — | 🗑️ no native verify |
| evict oldest idle at cap + SSE close | SEC2; SSE ⚠️ | ⚠️ port stream end |
| idle TTL reap frees slots | — | ❌ port (server.rs:369) |
| 413 over maxRequestBytes | SEC2 | ✅ |
| 413 chunked body | — | ❌ port |
| batch > maxBatchSize 400/−32600 zero-dispatch | impl :256 untested | ❌ port |
| maxConcurrentCalls caps batch peak | untested | ❌ port |
| principal evicts only OWN oldest | SEC2 | ✅ |
| busy session not reaped | impl :369 untested | ❌ port |
| 503+retry-after:5 all-busy | impl :404 untested | ❌ port |
| reap call past maxCallMs | N3 partial | ⚠️ port |
| principal → tool ctx + guards | napi user_id impl only | ⚠️ port |

## modern.test.ts (13) — 5✅(partial incl.) 5⚠️ 3❌ 5🗑️
| case | covering | verdict |
|---|---|---|
| per-request fresh actor, closed×1, abort | N1; stateless unsupported | 🗑️ |
| SDK1 client sessionful interop | none live | ❌ |
| legacy 2025-11-25/2025-06-18/2025-03-26/2024-11-05/2024-10-07 init+list | N4+N1; per-rev list unasserted | ⚠️ port loop |
| admission bounded → 503 | impl only | ❌ |
| disconnect can't bypass concurrency | impl only | ❌ |
| bearer `bearer `/`BEARER␣␣`/`Bearer\t` forms ×4 | exact-digest refuses | 🗑️ |

## projection.test.ts (15) — 5✅ 1⚠️ 2❌ 7🗑️
definition verbatim + keyset ✅ tool-identity:119+S1; no prefix ✅; outputSchema-not-forwarded ❌ (impl-only); isWriteTool ✅; drops/keeps writes ✅ S1/S2; two-block+summary-omit+envelope-fallback+modelData-selection+status-mapping 🗑️ TS-only; failure isError+structured ⚠️ port; partial→isError ❌; unserializable→errorCode ❌.

## request-cancellation.test.ts (2) — 2❌
abort mid-tool → ctx aborted + queued prevented (both modes): `handler.rs:97,121` impl-only → port.

## tool-source.test.ts (11) — 7✅ 2❌ 4🗑️
staged list/registry envelope/validator/unknown-refusal/timeout/deadline-untouched ✅; thrown-tool sentence-not-forwarded ❌; close-aborts+refuses-queued ❌; contributor-pruning/guard-chain/bindings 🗑️.

## auth.test.ts (10) — 5✅ 5🗑️
equality/lengths/refusals/same-length/empty-wiring ✅ (SEC1/S1/NP); unicode tokens, scheme-case accept, custom verify ×2, scheduling canonicalization 🗑️.

## dispatch-boundary.test.ts (2) — 2✅ (IB).

## extensions.test.ts (2) — 1⚠️ 1❌
N2 covers guesses/bad args; unknown prompt name + scope threading ⚠️; faults-private + absent-seams ❌.

## guard.test.ts (15) — 4✅ 3⚠️ 8❌ (guards.rs zero units)
SEC-verified: outside-list, origin mismatch, rebound-host, origin-when-present. Units needed: splitHostPort (lowercase/port/IPv6/malformed×5), any-port vs exact-port, origin trailing-slash, opaque/unparseable, wildcard, Origin-ignored-when-unconfigured, missing-header.

## inbound-actors.test.ts (5) — 2✅ 2⚠️ 1❌
distinct actors/revoke ✅ (S1/N1/SEC); approval-reuse + dedup-once ✅ host inbound-proposal; partial-repeat ⚠️; partial→isError+skipped ops ❌.

## Ports required
Rust (`rust/crates/mcp`) — port from the TS cases named above; tests must fail-before if the impl lacks the behavior (implement then, hardening-before-deletion rule):
1. guards.rs cfg(test): 8❌+3⚠️ rows.
2. batch cap 400/−32600 zero-dispatch; concurrent-call peak.
3. chunked 413, no session minted.
4. sessions: idle-TTL reap; busy-not-reaped; pinned-call reap past maxCallMs+TTL; all-busy 503 (+Retry-After if missing — implement); DELETE→404; foreign DELETE leaves owner live; eviction ends SSE GET; revoked-exact-once evict+close.
5. methods PUT/PATCH/OPTIONS 401→405; bad `mcp-protocol-version` session header 400.
6. cancellation both modes (sibling of IB).
7. results: ok:false/partial → isError+payload; backend Err hides tool text; napi to_call_result unserializable unit; no outputSchema in list.
8. features() gating absent+refused; no backend-error-text leak.
Native binding (`packages/native/tests`):
9. SDK1 live connect/list/call vs startMcpServer.
10. per-revision tools/list ×5 loop.
11. two same-token clients → distinct actors/run-ids; DELETE revokes only own.
12. admission 503 shapes at caps, no double dispatch.
13. www-authenticate: Bearer on 401 — implement or documented drop.
Host:
14. partial apply → already_applied with failed-op detail (inbound-proposal).

## Documented-drop list (record parity.md + mcp_server.rs header)
stateless per-request-actor mode; sessionScope header hook + chat-scoped catalogues; MCP-lane guard chain/bindings (already documented); custom verify(); lenient bearer scheme + scheduling canonicalization; unicode tokens; TS two-block/modelData envelope projection.

## Consumer inventory
root package.json :23/:38/:52; ci.yml :174–178 dist-existence + :189–191 api-extractor; packages/agentkit/package.json :55–58 + README :5,:59; scripts/build-umbrella.mjs:85; check-backend-parity.mjs:24; check-composition-ownership.mjs:43; node-smoke.mjs:48,110,126,~683–838 (step 8 incl. TS-client↔TS-server interop — replace server side or drop with note); pack-smoke.mjs:91,474+; umbrella-smoke.mjs:206+; qualification/consumer.mjs:19; qualification/types.ts:10,:21; docs/rust/api/{mcp-server,agentkit--mcp-server}.api.md + manifest.json; host/src/ports/tool-guard.ts:43 comment; docs/baseline + README refs.
