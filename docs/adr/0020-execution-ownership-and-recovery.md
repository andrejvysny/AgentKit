# ADR 0020: Durable execution ownership and recovery

## Safety boundary

Cancellation acknowledges a request to stop. It does not prove that a command
or its descendants have stopped. The coding contributor owns command cleanup
independently of the turn promise. Hosts must await contributor disposal and
propagate its failure before reporting successful shutdown.

A shutdown acknowledgment covers commands owned by the current contributor
graph, including cleanup adopted by its startup sweep. Observing another
owner's durable workspace quarantine does not transfer that command into the
current pack's cleanup lifecycle. The coordinator continues to block modifying
leases until verified recovery, including after an idle observing host exits.
Inspection errors still fail closed. Unresolved cleanup from commands started
or adopted by the current pack still prevents successful shutdown.

Process cleanup and execution authorization are independent. A new approval
does not release workspace quarantine. Successful cleanup does not replenish
an approval that has already been consumed.

## Command ownership

Shell commands require a coordinator that provides durable execution leases.
The default coordinator uses one directory per OS user on the same host,
keyed by canonical workspace path. Purely in-process or custom coordinators
without execution leases cannot admit shell commands. File-only consumers
may still use in-process coordination without claiming restart protection.

An exclusive workspace lease persists execution intent before spawning.
The intent binds the run, tool call and execution attempt. Process identity
and cleanup evidence update the same blocking marker. Cleanup returns
`stopped`, `survivors` or `unknown`; only `stopped` permits lease release.
Replacing the marker never opens an interval in which another managed writer
can enter. A dead owner or missing root PID is insufficient cleanup evidence.

Pack startup awaits orphan inspection. Unresolved executions, unreadable
ownership and legacy command locks remain blocked. Known legacy lock
directories can be supplied through `legacyDirs`; their records are retained.
Quarantine prevents cooperating AgentKit mutations, not unrelated editors.

Coordination assumes one host, OS user and PID namespace. All concurrently
writing application instances must use the upgraded shared coordinator.
An old application using its own private lock directory cannot be fenced by
a newer application. Historical commands whose environment markers, ancestry
and ownership records were already lost cannot be reconstructed reliably.
The runtime must not advertise complete arbitrary-process containment.

Commands run behind a fixed Bash guardian. The guardian releases the command
only after its identity is persisted, remains a process-group anchor after
the command exits, and freezes its group when the host control pipe closes.
This works in compiled Bun applications without invoking the application as
an interpreter. Exit codes preserve POSIX wait status; a status above 128 is
not guessed to be a particular signal.

Every signal revalidates the recorded PID and start identity. Missing inspection
evidence remains unknown. Darwin start times have second precision, and POSIX
inspection followed by signaling is not atomic. Detached descendants that
remove their marker between samples can escape the supported census.

`recoverWorkspace(root)` explicitly inspects persisted execution evidence.
It blocks active owners and incomplete historical records. For supported
guardian executions, it freezes and verifies a stable census, persists that
sealed evidence before any final kill, and never resumes those processes.
A second recovery can finish after a crash at a kill boundary. Only positively
verified cleanup releases quarantine. Immutable reclaim claims have recoverable
succession and pin the original inode; old claim files are not deletion authority.
This operation does not reset a host's missing native shutdown acknowledgment
or bypass a still-live owner. Those cases remain blocked.

## Durable authorization

The host and reference stores persist a fenced execution receipt before
invoking a mutating tool. The receipt binds operation arguments, execution
context, authorization generation and attempt. Only a newly successful
atomic admission can invoke the body; the database transaction ends first.
Stores without the required receipt capability refuse mutating execution.

An external effect and its receipt cannot be one atomic transaction. A crash
after admission but before a durable result therefore leaves an uncertain
operation, even when the body may never have started. Recovery requires a
fresh approval generation. That pending generation remains stable across
restarts and repeated Resume actions. Completed receipts restore their
recorded results without repeating the effect.

Unanswered approvals and answered, unused approvals survive restart.
Legacy running events without a result are treated conservatively; absence
of the new receipt table is not proof that an old command never ran.
Unreadable recovery evidence blocks execution instead of becoming an empty
uncertainty set. Receipt records are separate from model conversation history.

The host stamps `fenced-v1` on running events only for its own durable mutation
wrappers, identified internally rather than through tool or model metadata.
For these invocations, readable receipt absence proves that the body was never
admitted; a plan-mode refusal therefore does not create false uncertainty.
Unmarked legacy invocations retain conservative recovery. A marked retry cannot
erase an older uncertain operation merely by reusing its call ID; settlement
must match the operation, attempt/context, or its explicitly approved generation.

## Policy and qualification

The command policy normalizes supported literal paths, quoting and wrappers.
Supported inline interpreter evaluation requires fresh approval. High-risk
commands cannot receive standing grants. Ordinary Full-auto behavior remains
unchanged; a parser cannot prove all effects of arbitrary scripts or programs.

Standing grants bind bounded executable, environment, runner configuration,
dependency metadata and direct entry-point identity. They do not hash every
project source file or prove transitive behavior. See the coding package README
for the supported fingerprint boundary.

Direct preload, configuration and executable inputs are included when supported;
unresolvable direct inputs decline standing authorization. A remembered grant
is checked again after acquiring the exclusive lease, before execution intent.

Qualification combines deterministic admission/result barriers, real process
ownership tests, SQLite reopening and consumer tests against the exact
immutable artifact. Unit tests alone do not establish native Quit/relaunch
readiness. Native evidence and any remaining limitations belong to the
consumer's qualification record.
