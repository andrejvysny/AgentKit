# ADR 0038: Agent sandbox workspaces

Status: proposed (2026-11-24). Implements packages WP-1.1, WP-1.2,
WP-1.5 (the workspace service, 2026-11-25), WP-1.6 (the tool pack,
2026-11-25) and WP-1.7 (the runtime lane and failure codes, 2026-11-25),
and WP-0.6 (the backend decision, accepted 2026-11-25) of
the Agent Sandbox spec
(Milestone 1, backend-neutral core). Builds on ADR 0020 (durable
execution), ADR 0021 (layered crates) and ADR 0024 (repository ports).

## Context

Agents and chats need their own computer: a persistent volume, a disposable
isolated machine (microVM on macOS, container on Linux), default-deny
network, files in by copy and out by approved export. The guarantees that
matter (a command that may have started never runs twice, no two machines
write one volume, a fenced workspace admits nothing) must hold for every
backend, and the backend itself is not chosen yet: Milestone 0 picks Track A
(OpenShell) or Track B (own stack). The core therefore has to be written and
proven against ports and a fake.

## Decision

1. **Two new crates.**
   - `agentkit-workspaces`, layer 3 context: domain types (workspace,
     member, operation projection, grants, requests, limits, `RelPath`,
     `PathRoot`, `HostPort`), the two ports, their error enums, and the
     `WorkspaceService` (decision 10). No SQL, no backend, no store. It
     depends on `agentkit-kernel` (the `Runtime` port mints ids and is the
     service's clock; the ports never read a clock, time is always an
     argument), `tokio` (waiting and bounding backend calls) and
     `tokio-util` (the caller's `CancellationToken`).
   - `agentkit-toolpacks-sandbox`, layer 4 adapter: the `sandbox` toolset,
     guide and `SandboxPermissionGuard` (decision 14; no standing approval
     covers a sandbox call). It depends on `agentkit-workspaces`,
     `agentkit-governance` and `agentkit-core`, never on `agentkit-store`
     (R11; its end-to-end test uses the store as a dev-dependency only).
2. **`CONTEXT_DEPS["agentkit-workspaces"] = ["agentkit-engine"]`.** The
   ports name no engine type today, so the crate uses none; the entry leaves
   room for the service to reuse engine conventions without another gate
   change, as `agentkit-memory` does with its unused entries. It is not
   allowed to reach `sessions`, `chat`, `agents` or `governance`: tool
   admission and the ledger stay in the tool pack's path.
3. **Separate from the reserved Hybrid slots.** AGENTKIT-153 tied execution
   isolation to the Hybrid programme that owns `agentkit-automation` and
   `agentkit-remote`; both slots stay reserved and unchanged. Execution isolation is a
   property of a workspace (where an operation runs and what it may reach),
   not of remote execution (who runs a turn). A remote lane may later run
   turns that bind a workspace; nothing here depends on it.
4. **Two ports, each with an in-memory twin and one conformance suite.**
   - `SandboxBackend`: `describe`, `ensure_machine`, `dispatch`, `observe`,
     `cancel`, `read_file`, `list`, `stop_machine`, `apply_network_policy`,
     `drain_denials`, `storage_usage`, `reset_home`, `destroy_storage`. The
     fake in `agentkit-testkit` scripts faults (lost ack, hang, death
     mid-run, refused stop, stale generation, endless output); the backend
     suite runs INV-1..INV-9 on it and on every real backend.
     `ensure_machine` refuses every generation at or below one it
     answered `StopProof::Stopped` for, whether a machine of it ever ran
     or not: a slow start that arrives after the proof never runs (INV-5).
   - `WorkspaceRepository` (with `advance_and_unfence`, the one
     transaction that leaves a fence): workspaces (atomic create-or-get by key that
     seeds the default grants, F-17; a key is unique among records that are
     not deleted, so a deleted workspace's key can be used again), members, the operation projection,
     grants, requests and the per-workspace lease. The SQLite store and the
     in-memory twin both run the repository suite.
   - Errors are enums: `SandboxError` (with `code()` giving the result
     envelope codes) and `RepositoryError`, which `SandboxError` wraps.
5. **Fenced writes.** Every write that changes workspace or operation state
   takes a `LeaseToken` and `now`, and the repository checks it in the same
   transaction exactly as the engine checks a task `LeaseFence`: token and
   fencing number equal the stored lease and `expires_at > now`, else
   `LeaseLost` and no change. Fencing numbers increase per workspace across
   acquire, release and `break_lease` (recovery only). Owner-side writes
   (members, grants, request decisions, denial records) are not fenced.
6. **Inherited from the durable execution ledger (ADR 0020), not rebuilt.**
   The replay barrier (`before_guards`: a completed receipt is replayed, an
   admitted one without a result is uncertain and parks for a human),
   atomic admission, the result persisted before delivery, and operation
   identity: `execution_id = sha256(canonical {runId, toolCallId,
   approvalRequestId})`, compared with argument and execution-context
   fingerprints. `OperationId` wraps that id; the sandbox never derives its
   own (INV-2). The tool's effect context adds the run's pinned attachment
   (workspace key, kind, owner scope, folder) and agent to the fingerprint,
   never machine state (workspace id, generation, image digest): those
   change under a parked or redelivered call without changing what it was
   approved or recorded for (decision 14, "Effect context").
7. **New in this context.** The exclusive per-workspace lease with a wait
   limit and holder label (one writer per workspace across runs and agents,
   INV-4); the generation, advanced by compare-and-swap only after a
   `StopProof::Stopped` (INV-5), which a backend may give only on verified
   absence of the machine, never because it has no record of one; the fence, entered when a stop is unproven
   and left only through an explicit recover (INV-6); the operation
   projection with its phase rule (`Dispatching -> Running -> Terminal`,
   `Uncertain` settled only by a late receipt, a settled row written once);
   and `recover()` at start. The operation row is written before dispatch,
   so a missing row after a crash means "not dispatched" (F-1).
8. **Vocabulary: "fence".** The coding coordinator calls the same concept
   quarantine (a workspace blocked until cleanup is proven). Here the
   evidence is the backend's stop proof instead of the host process table;
   the word is `fence` throughout this context, and docs say it equals the
   coordinator's quarantine.
9. **Backend: Track B on both hosts (accepted 2026-11-25).** Milestone 0
   proposes the own stack over OpenShell; see "Backend decision (WP-0.6)"
   below for the measurements, crate list and layer changes. Its adapters
   are layer-4 crates; only `agentkit-runtime` names them.
10. **The workspace service (WP-1.5), as built.** `WorkspaceService` is
    constructed from the two ports, the kernel `Runtime` (clock and ids)
    and a `ServiceConfig`; it waits through a small `Pause` port
    (`TokioPause` in production; a test advances a manual clock instead,
    since the workspace's tokio has no `test-util`). Nothing is global; the
    only in-memory state is a cache of the machine handles this instance
    started (cap, idle stop, live policy, denial cursor), never truth.
    - **Inherited from the ledger, not rebuilt:** admission, the replay
      barrier (`before_guards`: a completed receipt is replayed, an
      admitted one without a result is uncertain and parks for a human),
      the cross-run uncertainty guard and barriers, argument and
      execution-context fingerprints, and the result persisted before
      delivery. The service receives the granted `execution_id` as its
      `OperationId` (INV-2) and never derives one. It does not decide
      what the model is told on a replay; it only guarantees that its own
      call never dispatches an id twice.
    - **New here:** (a) the call order: resolve (create at first use,
      D-36) -> a settled or uncertain projection row of this id is read
      back and nothing runs -> refuse fenced or deleted -> the workspace
      lease (wait 20 s, then `sandbox_busy` naming the holder) -> settle
      what blocks the workspace -> quota -> network policy -> the row as
      `Dispatching` -> dispatch -> observe with the last sequence,
      renewing the lease -> receipt written once -> release. Exactly one
      call site dispatches, reached only by the call whose
      `insert_operation` created the row (INV-1, INV-3). (b) A missing
      answer (dispatch error or timeout, observe error, unknown id, no
      receipt within 2 s of a cancel) makes the row `Uncertain` and stops
      the machine of its generation; the outcome is reported as uncertain,
      never as success or failure (SBX-07). A cancel is delivered after
      the ack, never by abandoning a dispatch in flight. Once
      `insert_operation` created the row the call answers an outcome,
      never an error: any later failure is reported as uncertain (the
      post-insert path returns no `Result`). Giving up writes the
      uncertain row first, then drains denials, then stops the machine,
      then fences an unproven stop. Once the row exists, the worker checks
      its lease before every step that may wait on the backend and renews
      it when no more than the longest such step is left (two call
      timeouts and a poll: a cancel, an observe and a poll, 40.25 s by
      default), and renews it right before the dispatch and right before
      each fenced write that ends the operation. `ServiceConfig::validate`,
      run by `WorkspaceService::new`, refuses a lease time-to-live that is
      not above that gap (60 s by default); the cancel grace does not
      count, since the watch keeps renewing through it. Before the row
      exists, and in owner actions (stop, recover, delete, the advance
      after a proof), the lease is renewed right before each fenced write;
      one that ran out answers `LeaseLost` as a refusal, nothing
      dispatched. (c) `LeaseLost` on any
      fenced write: the worker stops touching the operation (no write, no
      stop, no fence) and reports uncertain; the new lease holder adopts
      the open operation by observing it (a receipt settles it, a running
      one is cancelled and observed, no answer is uncertain), never by
      dispatching it. A fence write that fails after an unproven stop
      still reports uncertain (not fenced); the next use proves the stop
      again. The renewal right before the dispatch means a worker whose
      lease ran out after writing the row dispatches nothing. What
      remains is a dispatch that reaches the backend after the lease ran
      out, because the worker stalled (a suspended laptop) between that
      renewal and the call; closing it needs a fence epoch the backend
      checks (not decided). (d) Generations: a
      proven stop of an uncertain operation's machine leaves the workspace
      `Busy` with the uncertain row; the next use (a write, or a read that
      finds the lease free) asks the backend for a fresh proof and only
      then advances (INV-5); an owner stop, idle stop or cap eviction
      advances under the same lease as its proof; `recover` retries the
      proof, then advances and unfences in one transaction (INV-6); an
      unproven stop always fences. A fence records the newest generation that may still
      run (`WorkspaceState::Fenced { generation }`). A backend that
      refuses the current generation as stale gets one fresh proof and
      advance; one that serves a newer generation fences with that
      generation recorded ("another host may run it"). `recover` proves
      the recorded generation stopped (and the current one too when the
      recorded one is newer and the current one ran here), then moves
      past both and lifts the fence in one transaction
      (`advance_and_unfence`), so a crash can no longer leave the
      generation advanced and the fence in place. While fenced the
      service starts no machine, so a current generation above the
      recorded one never ran. An unleased read reloads the record after
      `ensure_machine`; a machine it started for a generation a stop left
      behind meanwhile is forgotten and stopped, and the read starts over.
      (e) Startup `recover_all`: one live service per host id is assumed.
      Lease holders carry the host id (`ServiceConfig::host_id`) and the
      boot id the service mints at construction; recovery breaks only
      expired leases and leases of earlier boots of its own host, and
      leaves a live lease of another host sharing the store, with its
      workspace, alone. Each open operation whose lease is free is stopped
      and made uncertain, or fenced on an unproven stop; a backend whose
      descriptor says it answers status after host loss is observed first
      and a receipt settles the operation (F-4). A lease without an open
      operation means nothing was dispatched (F-1) and is only broken.
      Then every other workspace that is neither deleted nor fenced and
      whose lease is free has its current generation stopped (machines
      left over from before the restart): proven, the generation
      advances; unproven, it is fenced only where an operation row shows
      a machine of that generation ran (the relaxation in (f)).
      (f) One relaxation: on a workspace that is not fenced and shows no
      evidence that a machine of its current generation started (no
      handle in this instance, no operation row of that generation), an
      unproven stop answers `NotStarted` and changes nothing; the backend's
      own refusal of `reset_home` and `destroy_storage` while a machine
      runs stays the guard. (g) Also new: membership for shared workspaces,
      the soft quota check before each mutation, the active-machine cap
      (least recently used idle machine stopped first, a busy one never)
      and idle stop, live grant and revoke, denials drained after each
      operation into requests, copy-in under `<folder>/input/` with
      numbered names (at most 100 probes, renewing the lease, unexpected
      probe errors propagated) that marks `holds_user_files` before the
      bytes can land, export reads (regular files, 64 MiB, digest; the
      kind check lists the parent with the normal 500-entry bound and
      refuses when the listing is cut before the file), and the output
      tail the caller sees (last 2,000 lines or 50 KB; full log at
      `.sandbox/logs/<op>.log`). (h) Bounds inside the host, whatever a
      backend returns: one observation is taken up to 4 MiB and the tail
      copies at most its own limit; a read is cut to the bytes asked for
      and a listing to the entries asked for; a command's log limit is
      cut to `OutputLimit::LOG`; one drain records at most 32 distinct
      targets (attempts counted), a workspace keeps at most 64 pending
      requests, and an outcome and the request lists report at most those
      counts. (i) Policy pushes to one machine are serialized and read the
      grants inside that lock, so a push that read the grants before a
      revoke never lands after the revoke's own push. The cap check, the
      machine start and its registration run under one in-instance lock,
      and a handle of an older generation never replaces a newer one in
      the cache. (j) The desktop quit, `quiesce` (F-13): admission closes
      (every new call answers `sandbox_unavailable`), running operations
      are cancelled and given the cancel grace for a receipt, which is
      recorded as `OperationPhase::Interrupted(receipt)` (else uncertain
      and the machine stopped), the service waits until every admitted
      call returned, then stops every machine it runs. (k) A delivery of
      an id whose row is open waits for the earlier delivery (lease free:
      adopt; row ended: read back) instead of answering `sandbox_busy`
      (F-6). A row read back reports the workspace's fence as it is now.
      The row stores the ledger's argument hash; the same id with another
      hash is `sandbox_operation_conflict`. (l) Delete proves the stop
      without advancing, destroys the storage, then marks the record
      deleted (advancing first only when an uncertain row keeps it busy).
      A failed destroy after a proven stop fences at the proven
      generation, which `recover` or another `delete` proves again; with
      no machine ever started it changes nothing. (m) A future dropped
      mid-operation (its caller gone) writes nothing more: its lease runs
      out and the next holder adopts the open row, observing and never
      dispatching it. A drop guard that releases the lease and records
      the row uncertain at once was not built: it would turn the tests'
      model of a host crash (an aborted task) into a clean shutdown.
11. **`PathRoot`.** `ReadFile`, `ListEntries` and `OperationKind::WriteFile`
    carry `root: PathRoot` (`Workspace` = `/workspace`, `Home` =
    `/home/agent`), so `sandbox_write_file` can write into `/home/agent`.
    A `RelPath` still cannot climb above its root. A binding's folder
    (`chats/<chat id>`) prefixes `Workspace` paths only. `ReadFile` also
    gained `raw: bool`, because an export must read binary files and a
    plain read answers a binary file's type and size without bytes.
    A command's working directory stays below `/workspace`; the guest
    creates a missing one (a chat's folder at its first use).
12. **`mark_ready` refuses an uncertain current generation.** Besides an
    open operation, `mark_ready` refuses with
    `RepositoryError::OperationUncertain` while an operation of the
    workspace's current generation is `Uncertain`, on `Busy` and on
    `Ready` alike, so "uncertain -> stop -> proven -> ready at
    generation + 1" is enforced by the repository and not only by the
    service. `advance_generation` stays allowed with an uncertain
    operation (it is the recovery path); after it, the older generation's
    uncertain operations no longer block. `unfence` is not changed.
13. **A denial after a rejection.** Owner decision: a refused connection to
    a target the owner rejected counts on the rejected request (its count
    grows, its state stays `Rejected`) and the owner is not asked again;
    the owner can still grant the target explicitly. The calling command
    is still told the host and port it was denied.
14. **The tool pack (WP-1.6), as built.**
    - **Gate.** `SandboxToolPack` contributes under namespace `sandbox` only
      when the host port `SandboxBindingResolver` answers a
      `SandboxBinding` (workspace binding, agent, mode) for the run; no
      schema has a workspace argument (SBX-11). Every body asks the
      resolver again and refuses `sandbox_not_attached` when the answer is
      gone or names another workspace or agent.
    - **Tools.** Six deferred tools in toolset `sandbox` (guide: versioned,
      at most 600 tokens, plus a session-static environment section built
      from `SandboxEnvironment` at pack construction, so the cached prompt
      prefix stays stable). Run, write and export are `DirectWrite`
      (`effect: write`); run and write record command start. Capabilities
      are `sandbox.exec`, `sandbox.write`, `sandbox.export`, `sandbox.read`,
      not `process.exec`/`fs.*`: the sandbox mode table is not the coding
      matrix (D-13), and the staging wrapper's `ExecutionRecheck` for
      `process.exec` re-checks after a lease wait that here happens inside
      the workspace service, out of the wrapper's reach.
    - **Operation id.** `agentkit-governance` hands the body the admitted
      execution id under `EXECUTION_ID_METADATA_KEY`
      (`"agentkit.executionId"`), set in `execute_receipted` (the common
      sink of the staged and the static wrapper) and removed for every
      call that was not admitted. Run, write and export refuse without it
      (no ledger, no write); the id is the `OperationId` (INV-2).
    - **Envelopes.** A user-level error is `ok: false`, `<code>: <message>`,
      `data.errorCode`. Every service refusal of run or write is reported
      in the ledger's not-started shape (`command_not_started`,
      `before_spawn`, `started: false`, the real code in `data.code`): the
      ledger hands each body an execution id no earlier delivery used, so
      no row of it can exist yet. `sandbox_operation_conflict` says a row
      exists and is never reported as not started. The store keeps
      no-start evidence (`complete_execution_not_started`, valid even after
      the task lease ended) only for tools that can prove it; that list
      now names `sandbox_run` and `sandbox_write_file` beside
      `run_command` and `run_tests`, else every refusal before the row
      would leave the receipt admitted, read as uncertain. An uncertain outcome is
      `tool_result_uncertain` with the spec's text ("outcome unknown:
      OneMind stopped the sandbox before the command reported back"),
      `fenced` when the stop was unproven, and is never the not-started
      shape. A refused connection is a `network_denied:` line naming host
      and port and the request recorded for the owner. The log path is
      printed absolute (`/workspace/.sandbox/logs/...`).
    - **Paths.** Relative paths resolve in the binding's folder;
      `/workspace/...` is taken literally and must lie in the folder (it
      is stripped before the service joins it again); `~/` and
      `/home/agent/` name the home tree (not for a command's directory);
      anything else is `path_outside_workspace`. Repeated slashes count
      once (`/workspace//a` is `/workspace/a`); collapsing them never
      changes which segment a `..` removes, so an escape is still refused.
    - **Mode table.** `SandboxPermissionGuard` applies the spec's table:
      read tools always; run and write refused in Read-only and Plan
      (final); export asks in Ask and Edit, runs in Full access (D-29);
      new hosts ask in every mode that may change. The live mode comes
      from an optional `CodingPermissionAuthority` (an `Err` fails closed),
      else from the binding (a chat turn is `SandboxMode::CHAT`, Ask before
      changes); unknown mode text is Read-only. The bodies apply the same
      refusals, so SBX-17 holds when a host forgets the guard.
    - **Export approval (F-14, SBX-08).** The guard reads the file and asks
      with path, title, size, type, SHA-256 and a text preview. Path,
      title and content are the model's choice, so the card escapes
      control characters, line and paragraph separators, bidirectional
      controls and invisible format characters in them (a backslash too,
      in path and title), puts path and title on labelled lines and the
      content last: a file name cannot add a line to the card. The title
      shown is the one delivered (the file name without one); a title over
      200 characters, or a path over 1,024 characters as escaped, is
      refused `invalid_arguments` before anything is asked. A guard has no
      tool call id and an `ApprovalGrant` carries no prompt, so what was
      shown is held in the pack, keyed by run, tool and argument hash.
      Stagings and asks are numbered from one counter: a body accepts only
      an ask made before its own staging (one a human could have answered
      before this attempt), and only when the answer the call carries did
      not exist yet when the ask was raised (an ask keeps the answers its
      staging carried; one raised by a resumed attempt, after a restart,
      was spent unseen and never counts). It takes the ask whatever it
      decides (single use), and exports only the bytes it read itself when
      their path and SHA-256 are the ones shown and the call carries its
      own one-use answer for these arguments. A changed file is `export_stale` (from the guard on
      resume, and from the body for a change after the guard's read); a
      call without an answer is `export_not_approved`, and so is an answer
      to an uncertain re-run (its card leads with the uncertainty and lists
      this export's preview under "This call also asks:", but the body
      still refuses to act on that answer): the re-run is refused and the
      model calls again, which asks with the plain card. The record is in
      memory: after a restart the body refuses and the model asks again.
      It serves every run of the host and is bounded: 1,024 runs, 64 asks
      per run, 1,024 asks in all. A run's record is dropped by last touch,
      a run holding no unanswered ask before one that does (a parked run is
      the least recently touched by nature); the total bound drops the
      oldest asks of the least recently touched runs. Two calls with the
      same arguments share one ask (the guard has no call id): the first
      body takes it, so a second body in the same attempt is
      `export_stale`. The loop parks one call per step, so through
      governance the second call asks for itself after the first resumed
      and exports after its own answer.
      The same call shape re-presents the stored ask byte for byte.
      Governance's `NO_STANDING_NAMESPACES` gains `sandbox`: no remembered
      yes covers an export (or anything else in the pack).
    - **Network approval.** When the workspace has pending requests not yet
      shown to the run, the run's next command or write asks "Allow
      connections to host:port from this sandbox." (one line per host,
      with the attempts). The body grants exactly the shown requests
      (`decide_request(Approve)`), only when the call carries an answer to
      these arguments that is not a re-run of an uncertain effect and was
      given after the card was raised, and only once. When the call's
      answer may not grant them (no answer, one older than the card, or a
      re-run's), the hosts count as not shown, so the run asks again. A host refused after the ask, or decided by the owner
      elsewhere, is not touched. Otherwise a shown host is asked again
      only by the same call shape. No standing policy: the grant itself is durable.
    - **Exports leave** through the host port `SandboxExportSink`, which
      receives the bytes, the call's execution id (to deduplicate) and a
      title; it answers a receipt or `export_refused` / `export_failed`.
    - **Effect context.** Write tools bind the run's attachment: workspace
      key, kind, owner scope and folder, and the agent. Not the workspace
      id, generation or image digest, and not the mode. Staging still
      reads the record with the non-creating `WorkspaceService::lookup`
      (D-36, F-16), only to offer nothing for a record of another kind or
      scope. An approval answered while its call was parked therefore
      holds when the turn's first command created the workspace in the
      same attempt, or an idle stop or a recover advanced the generation
      meanwhile. What the approval binds is checked where it is acted on:
      an export's bytes by the SHA-256 on the card, a host by its request
      still pending in the current workspace (a request of a deleted
      workspace is not found and nothing is granted), an uncertain re-run
      by the ledger's own uncertainty generation; the mode is read live by
      the guard and the body. A redelivery of a call that completed in an
      earlier generation replays its recorded result instead of being
      refused (the old refusal told the model to call again, which re-ran
      the command). Deleting the workspace and creating it again under the
      same key keeps the context: an approval or replay then belongs to the
      same chat or agent binding, which is accepted. The store's pending
      generation gate, keyed by context hash, now holds an uncertain
      operation across generations too. A context that does change (the
      host pinned another workspace or agent) is still refused:
      `execution_context_mismatch` on replay; on resume, staging's
      `tool_guard_refused` ("The approval no longer matches this execution
      context.") when a guard asks again (the sandbox guard re-presents a
      stored export ask, or a host ask while its request is pending),
      else the ledger's `approval_mismatch` at admission.

15. **The runtime lane (WP-1.7), as built.** `AgentRuntimeBuilder::sandbox_lane`
    takes a `SandboxLane` (public fields, like `NativeLane`): the
    consumer's `SandboxBackend`, `WorkspaceRepository`, `ServiceConfig`,
    `SandboxBindingResolver`, `SandboxExportSink`, an optional
    `CodingPermissionAuthority` and the `SandboxEnvironment`. `build()`
    refuses an invalid `ServiceConfig` before anything is spawned, builds
    the `WorkspaceService` on the runtime's clock, and registers the pack as
    a contributor and `SandboxToolPack::permission_guard()` among the guards
    (after the coding and subagent guards; it judges only `sandbox`). The
    authority is passed through as given: `None` judges by the binding's
    mode, so a chat turn stays Ask before changes even under a read-only
    profile. `recover_all` runs after the host's recovery and the subagent
    reaping, before the worker starts. A backend that cannot prove a stop
    fences the workspace (each stop waits up to the call timeout); only an
    error, a repository fault in practice, fails the build
    (`RuntimeError::Coded` with the sandbox code), because no sandbox call
    may run before recovery finished. The host reaches the service through
    `AgentRuntime::sandbox_service()` for `quiesce` at shutdown and the
    owner actions. `agentkit-runtime` depends on `agentkit-workspaces` and
    `agentkit-toolpacks-sandbox` unconditionally, as on the coding pack.
    Every code the pack and the service answer has a row in
    `core/src/failure_table_sandbox.rs` and in `docs/contracts.md`
    ("Sandbox tool codes"); `core/tests/failure_catalog.rs` scans the pack's
    `Refusal::new` and `refusal_text` codes, and the runtime's
    `tests/sandbox_lane.rs` checks every `SandboxError` and
    `ExportSinkError` code. No event type and no DTO changed.

## Consequences

- `check-arch` gains two `LAYERS` entries and one `CONTEXT_DEPS` entry; the
  allowlist does not grow.
- Schema version 23 (WP-1.3) adds the workspace tables behind the
  repository port; nothing outside `runtime` and `napi` names the store.
  The workspace row keeps the fence generation, the lease row the
  holder's host and boot ids, and the operation row the argument hash
  (and an interrupted operation's receipt). Version 23 is not released,
  so these columns are part of it, not a new version.
- `WorkspaceService::new` returns `Result<_, ServiceConfigError>`.
- `SandboxError::code()` introduces codes beyond the spec table
  (`sandbox_operation_conflict`, `sandbox_stale_generation`,
  `sandbox_not_found`, `sandbox_cancelled`, `sandbox_backend_failed`,
  `sandbox_store_failed`); they need failure-table rows when the tool pack
  raises them (WP-1.7). `sandbox_cancelled` means the caller cancelled
  before dispatch and nothing started.
- `sandbox_get_status` on a fenced workspace answers its status with the
  fenced state in it (it needs no machine); the tool pack presents it as
  a successful status that names `sandbox_fenced`. Every other read and
  every write answers `sandbox_fenced`.
- Governance's `PreGuard` gains `Reauthorized { generation }` (a public
  enum: an exhaustive match outside governance needs the arm), and an
  uncertain re-run's approval card leads with the uncertainty line (see
  the re-review bullet below).
- The tool pack (decision 14) adds `agentkit_governance::EXECUTION_ID_METADATA_KEY`,
  makes `WorkspaceService::lookup` public, extends the store's no-start
  tool list (`execution_no_start.rs`) and governance's
  `NO_STANDING_NAMESPACES`, and raises codes that need
  failure-table rows with WP-1.7: `durable_execution_required`,
  `plan_mode`, `permission_unavailable`, `export_stale`,
  `export_not_approved`, `export_refused`, `export_failed`,
  `write_failed`, `unknown_sandbox_tool` and the guard reasons
  `export_approval_required` and `network_approval_required`. WP-1.7 added
  those rows and the service's (decision 15).

- Governance uncertainty cards (re-review; supersedes the card notes above
  and under Not decided). An uncertain re-run's card leads with the
  uncertainty line (for a barrier without an argument fingerprint it adds
  that the earlier arguments were not recorded and may differ) and lists
  every other guard's preview under "This call also asks:" (at most 4,000
  characters, cut visibly), because its one yes answers those asks too. The
  request carries `cardKind: "uncertain"` in its hashed content
  (`CARD_KIND_KEY`, `UNCERTAIN_CARD_KIND`) and the id
  `hreq:<run>:<call>:uncertain:<generation>`, distinct from the older Rust
  `hreq:<run>:<call>:<generation>` and the TypeScript
  `...:generation:<generation>`; both are Rust only. Only an answer on such
  a card reauthorizes its generation (`ApprovalGrant::uncertainty_card`,
  checked by the ledger probe and by staging's one-use spend): a generation
  answered on an older card, whose prompt was another guard's preview,
  opens nothing and the call asks again. The card requests no mode, and
  `plan_request_of` ignores a generation-bound request, so the yes never
  escalates the session. When more than one unresolved execution blocks a
  call (this run, another run, a barrier), the call ends with
  `tool_result_uncertain` (`PreGuard::Refused`, `unresolvedGenerations` in
  the data) instead of asking: one answer names one generation, and the
  re-park used to land on an answered request and wait forever. `PreGuard`
  is `#[non_exhaustive]` and `Uncertain` gains `arguments_unknown`. The
  store checks only the marker's shape (`"uncertain"`, with a generation)
  and still admits a generation answer without it, for rows of older
  releases and TypeScript hosts. It does not enforce the marker: the Rust
  wrappers do, host-side (ledger probe, staging spend). The napi/TypeScript
  admit path does not enforce it either (known limitation). The sandbox
  bodies still grant no host and export nothing on a re-run answer, though
  the card now shows those asks.

- Service-level unknown outcomes and barriers (second re-review; refines
  the bullet above).
  - An operation the workspace service settles `Uncertain` (lost ack,
    timeout, observe error) used to come back as an ordinary `ok: false`
    result, which the ledger completed: a redelivery replayed it, but the
    same command under a new tool-call id ran again with no card (the
    probe blocks on admitted receipts only). Now `execute_receipted` does
    not complete the result of a body that records its command start
    (`Tool::records_command_start`) when it is `tool_result_uncertain`
    with `uncertain: true`: the receipt stays admitted, which the ledger
    already reads as uncertain, so the same operation under any call id
    asks on the uncertainty card (or is refused where the host cannot
    park) and one yes runs it once. One mechanism, no new receipt status.
    The result carries `approvalGeneration` (the operation id, which is
    the execution id) next to the spec's text. `Interrupted` (a receipt
    read within a quit's grace) is a proven end and completes as before.
    Two consequences: a redelivery of the same call id now parks on the
    card instead of replaying the unknown outcome (spec: "Model retries
    the same call"); and the first receipt stays admitted after an
    approved re-run, so a later run of the chat asks about it again
    (ADR 0020's cross-run guard) until an operator resolves it. The
    ledger's schema-refusal recorder repairs only a row admitted for its
    own tool and arguments: it used to complete any admitted row of the
    call id in this attempt, which is now also a body's unknown outcome, so
    a schema-invalid repeat of that call id would have closed it.
  - Every matching barrier counts in the probe. It used to take only the
    newest while the store's admission gate holds every barrier, so with
    two the card named one, the gate refused each yes
    (`execution_already_admitted`) and every yes ran nothing. Two or more
    unresolved executions end the call (`PreGuard::Refused`) with the ones
    an operator can resolve now first (`approvalGeneration` names the
    first); this run's own receipt comes last and the message says it can
    be resolved only after the run ends (`receipt_run_active`).
  - Operator route for a barrier:
    `SqliteStore::resolve_uncertainty_barrier(chat_id, barrier_id)` and
    napi `SessionService.resolveUncertaintyBarrier({chatId, barrierId})`
    (TypeScript `resolveUncertaintyBarrier`) remove one barrier the chat
    sees (its own, one of a run bound to it or of a task in its scope, or
    an unattributed one) and answer it; a missing barrier and another
    chat's are both `not_found`. No active-run check: a barrier is import
    residue no live run can complete. Not through `SessionRepository` (the
    in-memory twin has no barriers). Limits: the barrier table has no audit column, so
    the decision is not recorded in the store (the host records the
    answer), and an import run again over the same TypeScript source
    recreates the barrier (insert-if-absent on a deterministic id). A
    tombstone column would close both and needs a schema version.
  - An uncertainty card shows every other ask: one without a preview
    (coding `approval_required`, `high_risk_command`) by the first line of
    its reason, at most 200 characters, else `Approve <tool>?`.

## Backend decision (WP-0.6)

Status: **accepted by the owner (2026-11-25).** The evidence is in `docs/evidence/sandbox-m0/`:
`wp-0.3-openshell-desktop.md`, `wp-0.4-openshell-server.md`,
`wp-0.5-virtualization-framework.md` and `wp-0.5-libkrun.md`. The spike
code lives outside the repository and may be deleted; the reports carry
the numbers.

### Which branch of the decision rule applies

Neither host passes every Must on Track A (OpenShell 0.1.10):

| Must | Desktop (WP-0.3) | Server (WP-0.4) |
|---|---|---|
| A-1 runs as our child, no Docker | **fail**: needs a Docker engine or e2fsprogs | — |
| A-3 survives image update | migration only | in place on Docker; copy migration on Podman |
| A-8 offline, our image only | **fail**: contacts the registry before its cache | pass |
| A-11 no VM survives OneMind | **fail**: gateway and VM orphaned; relaunch starts a second VM on the same overlay | — |
| A-13 supported hosts | — | pass on Ubuntu 24.04 and Debian 13 VMs; LXC unsupported (no Landlock, runc sysctl error, no tun) |
| A-14 sole socket holder | — | pass (mTLS instead of a token) |
| A-15 SDK builds, `cargo deny`, upgrade drill | **fail**: git sources, `axum` ban, chrono advisory; drill blocked (0.1.10 is the newest tag) | same SDK, same failure |

A-15 belongs to the SDK, so the server shares it; the server is therefore
not a clean pass either. The rule's "server passes, desktop fails" branch
defaults to Track B on both, and with the server also failing a Must that
default only gets stronger. The owner may overrule with the server
evidence (GO with conditions: Docker 28+, push config delivery, `/sandbox`
as the only persisted path, no per-sandbox disk quota).

### Track B measurements (macOS helper candidates, WP-0.5)

Both spikes booted the v0 root disk read-only, attached a read-write sparse
workspace disk, exchanged a framed hello over vsock with no network device,
ran commands as uid 1000 and stopped. 2 vCPU, 2 GiB. macOS 26.6.2, Apple
Silicon, ad hoc signatures.

| Measure | Virtualization.framework | libkrun 1.19 |
|---|---|---|
| Start to first result, warm | 318–350 ms | 131–159 ms (median 136) |
| Further command, p50 | 0.52–0.76 ms | 0.51–0.58 ms |
| Graceful stop | 141–178 ms | 2–7 ms |
| Idle memory, one VM | helper 17 MB + VM service ~150 MB (`footprint`) | one process ~154 MB (`vmmap` phys_footprint; `footprint` shows 63 MB) |
| Memory freed by the guest while running | not returned (balloon inflates, footprint stays) | returned with free-page reporting (REPLACE mode) |
| Memory on stop | returned | returned |
| A-11 parent kill | VM gone in 0.21 s, graceful | helper stops guest, gone in 25 ms |
| Second VM on same workspace disk | refused (macOS disk lock plus sidecar `flock`) | **no lock**; two writers possible (untested) |
| Signing (A-1) | one entitlement, `com.apple.security.virtualization`; no third-party code | `com.apple.security.hypervisor`; hardened runtime rejects the ad hoc dylib unless `disable-library-validation`, one Team ID, or static link (untried) |
| Third-party runtime code | none (Apple framework) | libkrun (Apache-2.0, 84 crates) plus libkrunfw (LGPL-2.1) or our own kernel; 2.0 API announced |
| Boot configuration | `VZLinuxBootLoader`: kernel, initramfs, command line as files | bundled kernel forces `rootfstype=virtiofs init=/init.krun`; external `Image` via `krun_set_kernel` needed |
| Helper size, owned code | 519 KB, 531 lines of Rust | 403 KB + 4.85 MB dylib, 409 lines of Rust (10 `unsafe` blocks) |

Not like for like: the spikes used different kernels (Debian 6.12 cloud
with an initramfs; libkrunfw's kernel without one), different memory tools,
and libkrun's guest was a Python stand-in for `agentkit-sandboxd`. Both
pass A-4 (15 s, 300 ms) by two orders of magnitude, so start time does not
decide. Neither ran on macOS 15, on a clean machine, or with a Developer
ID; notarization is blocked by WP-0.1.

### Choice: Virtualization.framework, provisional

It wins on the Musts; libkrun wins only on Shoulds.

- **A-1:** an Apple framework and one entitlement, against a third-party
  dylib that needs library validation relaxed or a Team ID we do not have.
- **Workspace safety:** the OS refuses a second VM on the same disk; libkrun
  would need our own lock before it is as safe.
- **A-15 analogue:** no third-party runtime code inside the signed app, no
  crates.io build of a C-ABI library, no announced API break.
- **Against it:** about 200 ms slower to start, two processes per VM, and
  memory freed by the guest is not returned until stop (A-5 is a Should and
  asks only for return on stop).

Kernel ownership is the same on both: either way OneMind configures,
builds and ships an arm64 kernel and owes its GPL-2.0 source. The kernel is
built in CI with `virtio_blk`, vsock and balloon built in (no initramfs) and
is versioned with the image manifest.

libkrun stays the fallback. The choice is re-checked before Milestone 3 on a
clean macOS 15 machine with a Developer ID build and the real musl
`agentkit-sandboxd` as PID 1; it gates Milestone 3, not Milestone 2.

### Crate list for Track B

`agentkit-sandbox-openshell` leaves this ADR. The server crates and their
design are in ADR 0039: `agentkit-sandbox-proto` (layer 1),
`agentkit-sandboxd`, `agentkit-egress`, `agentkit-sandbox-launcher` and
`agentkit-sandbox-container` (layer 4). `agentkit-sandbox-microvm` (layer 4,
FFI and `unsafe` in `sys` or the helper) follows in Milestone 3.

### Layer table changes

- `LAYERS` gains the five server crates (WP-2.1) and, in Milestone 3,
  `agentkit-sandbox-microvm`. `CONTEXT_DEPS` is unchanged: none of them is
  a layer-3 context.
- The public-address rules move from `toolpacks-web/src/fetch/ssrf.rs` to
  `agentkit-kernel::net` (pure functions, no I/O), so the egress proxy and
  the web tool pack share one set of rules without one adapter depending on
  the other.

### Relation to the reserved Hybrid slots

Decision 3 still holds. The launcher is a deployment service that only
creates fixed containers; it does not run turns, so it is not
`agentkit-remote`, and nothing schedules work, so it is not
`agentkit-automation`. Both slots stay reserved and unchanged.

### Spec corrections the spikes surfaced

Still true on Track B:

- A pip grant needs `pypi.org` and `files.pythonhosted.org`; the pip preset
  must name both.
- A-9's "DNS fails" should read "DNS gives no upstream resolution": a
  resolver that answers locally without querying upstream leaks nothing.
  On Track B the guest has no network device, so DNS fails as written.
- A per-workspace disk quota does not come from the runtime; on Track B the
  workspace disk's size is the quota.

Moot with Track A gone: push config delivery for the 5 s revoke,
`/sandbox` as the only persisted path (the image keeps
`WORKDIR /workspace`), mTLS instead of a token.

## Not decided

- A fence epoch the backend checks on `dispatch` (the lease's fencing
  number, refused when lower than the highest seen), to close the window
  of a dispatch that reaches the backend after the worker's lease ran out.
- A drop guard for a dropped mutating call (see decision 10 (m)).
- More than one live service per host id; the lease's host and boot ids
  assume one.

- The macOS helper technology is provisional (see "Backend decision"); it
  is confirmed or switched before Milestone 3.
- Whether `unfence` should also refuse an uncertain current generation, as
  `mark_ready` now does. The service never calls it (`recover` uses
  `advance_and_unfence`, which moves past that generation), but another
  caller of the port could.
- An idle machine that becomes unreachable answers `sandbox_unavailable`
  on every use until the owner stops it; whether the service should stop
  it and start the next generation by itself.
- Orphan machines from before a restart that ran no operation (started
  for reads) are stopped at start only when the backend can prove the
  stop; an unproven one is not fenced (no evidence it ran). WP-5.2 owns
  the orphan sweep.
- Tool pack: what an ask showed is kept in memory, so an approval answered
  across a restart is refused and asked again; a durable record would
  need a repository change. Governance shows one card per approval
  request; for an uncertain re-run it leads with the uncertainty line and
  lists every other guard's preview, so the one generation-bound yes is
  given knowing what else it answers (re-review bullet above). The sandbox
  bodies still refuse to act on it, and the card requests no mode, so a
  plan approval cannot escalate on it. On resume
  the ledger reports the covered generation (`PreGuard::Reauthorized`)
  and staging spends exactly that answer when a guard asks again, so an
  approved re-run runs once instead of asking forever (this loop also hit
  the coding pack in Ask mode). The pack grants no host and exports no
  content on such an answer.
  Approvals of a run or write survive an idle stop's generation advance
  (the effect context binds no machine state, decision 14). New-host asks
  are raised on run and write only.
  The export guard reads the whole file (up to 64 MiB) on every
  evaluation, without a hash cache (one keyed by size and mtime could show
  a stale card), within the
  host's guard timeout (15 s by default in chat, `contribute_ms`), and the
  read may first start the machine. A denied hosts card leaves its
  requests pending: they are not asked again in that run, but every new
  run asks once. The `sandbox` toolset is registered (and its guide is
  loadable by id) even for a run that is offered none of its tools;
  `toolsets()` has no run context. A body's own unknown outcome reuses the code
  `tool_result_uncertain`, with `uncertain: true` and its
  `approvalGeneration` (see the second re-review bullet).
- Governance: `memory_forget`'s body does not refuse a re-run answer the
  way the sandbox export does (the memory context may not depend on
  governance, and its lane lives in `runtime`); it relies on the
  uncertainty card listing the record. `run.waiting` does not carry
  `cardKind` (core copies a fixed key list; adding it is a wire change).
- Follow-ups noticed in the review, not fixed:
  - Service errors other than a conflict map to not-started, so a
    redelivery or adopt path could report an error after the row exists.
    Unreachable today: ledger ids are unique (`execution_receipts`
    `ON CONFLICT DO NOTHING` and the unique approval index). A typed "row
    exists" error is suggested.
  - The TypeScript native loop merges `...input.toolMetadata` after the
    Rust metadata, so a host can override `agentkit.executionId` seen by JS
    tool bodies. No JS tool reads it today.
  - Governance `resolve_approval` with a context-hash mismatch under a
    re-asking guard returns `tool_guard_refused` with next step "Do not
    repeat this call", which contradicts the `approval_mismatch` wording.
  - `memory_forget`'s body does not refuse uncertainty grants (above).
  - No standing approval policy keyed by workspace, tool and host (spec
    WP-1.6 asked for one); owner decision pending. Today `sandbox` is in
    `NO_STANDING_NAMESPACES`.
  - The export guard reads the whole file (up to 64 MiB) per evaluation.
  - The lane's authority cannot be late-bound before build
    (`SessionExecutionAuthority` needs the session service).
  - The probe reads only this run's newest receipt of an operation: once
    an approved re-run of an unknown outcome completes, even as not started
    (`sandbox_fenced`, `sandbox_busy`), the older admitted generation no
    longer asks in this run (one yes still opens at most one dispatch);
    later runs ask about it again (cross-run step). Existing ledger rule.
