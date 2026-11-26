# ADR 0039: Sandbox Track B server backend

Status: proposed (2026-11-25). Implements WP-2.1 of the Agent Sandbox spec
(Milestone 2) under the backend decision recorded in ADR 0038
("Backend decision (WP-0.6)", accepted 2026-11-25). Builds on ADR 0021
(layers) and ADR 0038 (the `SandboxBackend` port).

## Context

ADR 0038 chose our own stack (Track B) on both hosts. On a Linux server a
sandbox is a container started from a fixed template by a small launcher
service that alone holds the container runtime socket. Inside it a
supervisor, `agentkit-sandboxd`, runs commands and file operations for the
host over a framed protocol. The container has no network; the only way out
is a host-side CONNECT proxy that applies the workspace's grants.

## Decision

### Crates

| Crate | Layer | Kind | Owns |
|---|---|---|---|
| `agentkit-sandbox-proto` | 1 | lib | Frame codec, guest messages, launcher messages, `PROTOCOL_VERSION` |
| `agentkit-sandboxd` | 4 | bin + lib | Guest supervisor (Linux; portable core testable on macOS) |
| `agentkit-egress` | 4 | lib | CONNECT proxy on one unix socket per machine |
| `agentkit-sandbox-launcher` | 4 | bin + lib | Fixed-template container service over the Docker or Podman API |
| `agentkit-sandbox-container` | 4 | lib | `SandboxBackend` over the launcher, the guest channel and egress |

- `agentkit-sandbox-proto` depends on `serde`, `serde_json` and `thiserror`
  only; no tokio. It holds wire types, not port types: the adapter maps
  them to `agentkit-workspaces` types.
- The public-address rules move out of `toolpacks-web/src/fetch/ssrf.rs`
  into `agentkit-kernel::net` (pure functions on `IpAddr`, no I/O);
  `toolpacks-web` and `agentkit-egress` both call them. One set of rules
  for every egress path.
- New `unsafe` (`prctl`, `openat2`, `setsid` helpers) goes in `agentkit-sys`.
- Only `agentkit-runtime` names `agentkit-sandbox-container` (later WP).

### Topology on a server

```
onemind-server ──unix──▶ launcher ──unix──▶ Docker / Podman API
     │  listens per machine:                      │ creates
     │  <sock>/<machine>/control.sock             ▼
     │  <sock>/<machine>/egress.sock      sandbox container (no network)
     ◀────────── dials control.sock ────── agentkit-sandboxd (PID 1, uid 1000)
     ◀────────── egress via shim ───────── 127.0.0.1:3128 shim ◀── workload
```

- `<sock>` is a host directory configured in both the launcher and the
  adapter (the adapter sees it at its own mount path). Each machine gets
  one flat directory below it, `<machine>` =
  `agentkit_sandbox_proto::machine_dir_name(ws, generation)`: `m` and 23
  hex digits of a SHA-256 of the workspace id and generation, 24 bytes
  whatever the id and generation, so the socket paths always fit a unix
  socket address (`sun_path`, 104 bytes on macOS, 108 on Linux) for any
  `<sock>` up to 65 bytes (69 on Linux). The launcher and the adapter both
  derive the name; neither trusts one from the other. The launcher
  bind-mounts only `<sock>/<machine>` into the container, read-only, at
  `/run/agentkit`. Another workspace's sockets are not visible (NET-7).
- **Permissions.** `<sock>` belongs to the launcher (root under Docker,
  the rootless user under Podman) and grants others nothing: mode 0710
  with group = the server's gid when the server is another user (Docker),
  0700 when the launcher runs as the server's user (rootless Podman). No
  other host user can reach a machine's sockets, and nobody but the
  launcher can create, rename or replace a machine directory. A machine
  directory belongs to the server's uid, mode 0711: the container sees it
  as its mount root, so the container's uid (1000, or the subuid under
  Podman) needs only search permission on it and never traverses `<sock>`
  (WP-2.1 probes, section 4: a bind source below a 0700 parent works for
  rootless Podman; under Docker the 0710 parent was observed to work on both
  probe VMs, see `wp-2.1-conformance.md`, "After the security fixes"). Each socket is 0666 (the
  container's uid must connect); there is still no peer-uid check
  (rootless Podman reports a subuid).
- The launcher validates the workspace id (`[a-z0-9-]`, bounded length) and
  the generation before it builds any path or volume name, and it creates
  the per-machine directory itself (never following a link), owned by the
  configured server uid so the adapter can place its two sockets in it. A
  caller can never choose what is mounted.
- **Ancestors.** `<sock>`, the launcher's state directory and its listen
  socket's directory are opened from `/` one component at a time without
  following links and checked by descriptor, at startup and, for `<sock>`
  and the state directory, again at every container start: every
  directory above them must be
  owned by root or the launcher and not writable by group or others (a
  sticky world-writable directory such as `/tmp` is refused too), or
  another user could rename the directory away and substitute their own
  between the check and the runtime's lookup of the mount source. The
  state directory follows the same rule; the listen socket's directory
  must belong to the launcher itself, so nobody can replace the socket
  between its `bind` and the mode change that follows by path.
- The host side listens; the guest dials out. A read-only bind mount still
  allows `connect`, so the workload cannot replace a socket file.
- **Channel token.** The launcher puts a fresh random token (a v4 UUID, 32
  hex digits) in each container's environment (`AGENTKIT_CHANNEL_TOKEN`)
  and answers it in `ensured`; it stores nothing else, and an `ensure` of
  the running generation reads the token back from the container's
  recorded environment (`Config.Env` of the Engine API inspect; read
  through Podman's compat API is not yet verified on a VM). A running
  container without a valid token is `busy`. The token never appears in a
  log line or an error.
- **Handshake.** The adapter accepts control connections until one
  answers `hello` with the protocol, the generation, the image digest and
  the token (compared in constant time), all within the handshake time.
  Candidates are greeted concurrently, so a peer that connects first and
  stays silent cannot hold the guest out; peers that answer wrong are
  dropped; at most 16 connections are taken. Then the listener is closed:
  a later connection (after the supervisor died) is refused and the
  machine is treated as unreachable. The supervisor starts before any
  workload and does not pass the token on (its commands get a cleared
  environment), so the workload cannot take the channel. A process that
  reaches the socket some other way (under Docker, a host user of the
  container's uid, through `/proc/<pid>/root`) must also present the
  token, which lives only in the supervisor's environment and the
  runtime's container record; the supervisor makes itself non-dumpable
  first thing, so `/proc/1/environ` is closed to such a process from then
  on (the instant between its exec and that call is not closed; inferred,
  not tested).
- **No host-loss survival.** When the control connection closes, the
  supervisor ends every process and exits, and the container stops.
  `supports_status_after_host_loss` is `false`: a lost ack is uncertain, and
  after a host restart `stop_machine` proves the old generation gone by
  label. This mirrors the VM helper's parent-pipe rule.

### Guest protocol (`agentkit-sandbox-proto`)

- Frame: `u32` big-endian length of the rest, then a `u8` kind.
  - Kind 1, control: UTF-8 JSON, at most 64 KiB.
  - Kind 2, data: `u64` request id, `u64` sequence, raw bytes, at most
    1 MiB of payload.
  - Any other kind, an oversized length or invalid JSON closes the channel.
- Control envelope: `{ "id": u64, "body": { "type": ..., ... } }`. Replies
  carry the request's id. Requests may be in flight concurrently (observe
  and reads beside a running command).
- Messages, as in the spec: `hello`, `exec`, `write` (then data frames,
  last one empty), `status`, `output`, `cancel`, `read`, `list`, `usage`,
  `shutdown`; replies `hello`, `accepted`, `conflict`, `state`, `unknown`,
  `file_data` and `output_data` (control header with `len`, then data
  frames ending with an empty one), `entries`, `usage`, `bye`,
  `error { code, message }`. `exec` carries `timeout_ms` (a duration, so
  host and guest clocks need not agree).
- `hello`: the host sends `{ protocol, generation }`; the guest checks both
  against `PROTOCOL_VERSION` and its own generation (from the launcher's
  environment) and answers `{ protocol, generation, image_digest,
  channel_token }` or closes. The adapter checks the digest against
  `MachineSpec` and the token against the launcher's `ensured`.
- Output sequence numbers are byte offsets in the operation's log.

### Supervisor (`agentkit-sandboxd`)

- PID 1 of the container (no separate init), uid 1000, non-dumpable,
  child subreaper, control descriptor close-on-exec.
- Reads its generation, image digest, channel token (1 to 256 printable
  ASCII characters) and socket path from the environment the launcher
  sets; dials `control.sock`, handshakes, serves. If the
  socket does not accept within 30 s, or the handshake fails, it exits.
- Operation table in memory for its generation: same id and same args hash
  returns the state; another hash is `conflict`; an unknown id is
  `unknown`.
- `exec`: clean environment from `/etc/sandbox/environment` (plus `TZ`
  when given), `bash -c` with `BASH_ENV` set by that file (the image's rc,
  never the home's), working directory `/workspace/<cwd>` created as
  needed, its own session. Full output (stdout and stderr interleaved) goes
  to `/workspace/.sandbox/logs/<op>.log`; passing `output_limit` ends the
  command as `output_limit`; the deadline ends it as `timed_out`; `cancel`
  sends SIGTERM, then SIGKILL after 2 s.
- **When an operation ends, every process other than the supervisor is
  killed**: as PID 1 it repeats `kill(-1, SIGKILL)` and reaps until no
  child is left, so a process forking during the sweep cannot escape it.
- Every path the supervisor touches below `/workspace` or `/home/agent`
  (the write target, the exec working directory, the log file) is resolved
  one component at a time without following links (the `agentkit-sys`
  nofollow helpers), because both trees are writable by the workload. The
  log file is created with `O_EXCL | O_NOFOLLOW`. This matters most in the
  VM, where the supervisor is root.
- `write`: temp file in the target directory, SHA-256 check, `fsync`,
  atomic rename, mode bits from the request.
- `read` and `list`: descriptor-based under `/workspace` or `/home/agent`
  with the same per-component resolution; a link is listed, never
  followed.
- `usage`: bytes used under `/workspace` and `/home/agent`, and
  `complete: false` when part of either tree could not be measured.
- `shutdown`: ends all sessions, `sync`, answers `bye`, exits 0.
- Egress shim: listens on `127.0.0.1:3128` inside the container and pipes
  each connection to `/run/agentkit/egress.sock`. The image presets
  `HTTP_PROXY`, `HTTPS_PROXY`, pip and npm proxy settings. Programs that
  ignore them fail closed (spec R-11).

### Launcher (`agentkit-sandbox-launcher`)

- Listens on one unix socket; speaks length-prefixed JSON frames from
  `agentkit-sandbox-proto::launcher`. API: `ensure`, `stop`, `inspect`,
  `usage`, `reset_home`, `destroy`. Nothing else: no caller-chosen image,
  mount, device, flag or network.
- Talks to the runtime API over its unix socket (Docker Engine API; Podman
  through its Docker-compatible service). No CLI.
- Fixed template: image by digest from the launcher's own config;
  `NetworkMode: none`; `CapDrop: ALL`; `no-new-privileges`; read-only
  root; tmpfs `/tmp`; user `1000:1000`; pids, memory and CPU limits by
  tier; two named volumes per workspace, `aks-ws-<ws>` at `/workspace` and
  `aks-home-<ws>` at `/home/agent`; the socket directory read-only at
  `/run/agentkit`; labels `agentkit.sandbox.workspace` and
  `agentkit.sandbox.generation`; no restart policy.
- Generations: `ensure` refuses a generation at or below one it has proven
  stopped for that workspace. It persists `stopped_through` and
  `created_through` per workspace in its own data directory (atomic write,
  under an exclusive lock on that directory, so a second launcher instance
  exits before it binds), and the rules survive its restart.
- **One container per generation, ever.** `created_through` is recorded
  before the create request. A generation whose container failed, exited or
  was removed is never started again (`busy`); recovery is a proven `stop`
  and the next generation. The supervisor's operation table lives in
  memory, so a generation run twice would forget operations it executed.
  A running container of a different generation is refused, not replaced.
- Only the server's uid or the launcher's own may connect (peer
  credentials). A request's tier must equal a configured tier by value.
- At most `max_containers` containers (default 8, any state, all
  workspaces) exist at once. A create beyond it is refused with
  `runtime_failed` and records nothing, so the generation can start later;
  the count and the create run under one launcher-wide lock. (`busy` would
  make the adapter answer the generation stale, and the service would burn
  generations and fence the workspace over a full host.)
- `stop` of generation G covers every container of the workspace at or
  below G, and every one whose generation label is missing or unreadable:
  stop with a grace period, then remove; answers `stopped` only when the
  runtime lists none of them (and no container of their names exists), and
  only then records `stopped_through`. A higher generation's container is
  left alone. A stop removes the machine directories of the generations it
  covered, a create the one of the generation created before (nothing of
  the workspace exists then), and `destroy` the one left.
- After its own restart it finds containers by label.
- Runtime facts it relies on (`docs/evidence/sandbox-m2/wp-2.1-runtime-probes.md`,
  Docker 29 and rootless Podman 4.9 and 5.4):
  - The image is referenced as `localhost/agentkit-sandbox@<index digest>`
    (tagged at install time); the only form both runtimes accept. A
    container's image is verified by `.Config.Image` and a label, never
    `.Image` (Docker reports the index digest, Podman the config digest).
  - `PidsLimit` is always sent and positive (Podman maps 0 to 1),
    `MemorySwap` equals `Memory`, `Init` is false, and `/tmp` and
    `/var/tmp` get explicit tmpfs size and `noexec` options.
  - Volumes use the default copy-up (the image's empty `1000:1000`
    directories make them empty and writable); never `NoCopy`, which leaves
    the root unwritable on Docker and is ignored or refused by Podman.
  - The machine directory is mode 0711 and each socket 0666; no peer-uid
    check (rootless Podman reports a subuid). The machine directory's
    parent need not be searchable by the container (section 4).
  - Volume size comes from `/system/df` (apparent bytes, under 1 s cold);
    the hot path is the supervisor's `usage`.
- `reset_home` recreates the home volume; `destroy` removes both; both
  refuse while a container of the workspace exists.

### Egress proxy (`agentkit-egress`)

- One listener per machine on `egress.sock`; the socket a connection
  arrives on is the workspace identity. No TCP listener.
- Accepts `CONNECT host:port` and plain HTTP with an absolute `http://` URI
  on port 80 (one request per connection, `Connection: close` forced).
  Anything else gets `400` and closes. The request head is bounded in size
  and read time.
- Order: policy check on the exact `HostPort` (NET-2, NET-4); resolve the
  name on the host (NET-3); drop every address `agentkit-kernel::net`
  refuses; connect to the first remaining one. A literal IP address is
  never granted by a host-name grant.
- Every refusal (not granted, refused address, limit) becomes a denial
  with host, port, time, reason and the operation the adapter marks as
  current (NET-5), kept in a bounded buffer read by cursor.
- `apply_policy` takes effect for new connections at once and closes open
  tunnels to hosts no longer granted (NET-6, well under 5 s).
- Limits (NET-8), configurable: concurrent tunnels per machine, tunnel
  lifetime, bytes per tunnel. Logs carry host, port, byte counts and
  timing, never content (NET-9).

### Backend adapter (`agentkit-sandbox-container`)

- Implements every `SandboxBackend` method over the launcher client, one
  guest channel per machine and one egress listener per machine.
- `describe`: kind `container`, `supports_status_after_host_loss: false`,
  `enforces_disk_quota: false` (the service checks usage before each
  mutating operation; hard volume quotas are a deployment matter recorded
  in the Milestone 2 quota matrix).
- `ensure_machine`: asks the launcher (which creates the socket directory
  and starts the container), binds both listeners in that directory, waits
  for the handshake (15 s; the supervisor retries its dial for 30 s), and
  applies the policy.
- A channel failure marks the handle unreachable; calls then answer
  `sandbox_unavailable`, and the service fences on the next stop as it
  already does. `ensure_machine` of an unreachable machine's generation
  answers `stale_generation` at that generation (as a `busy` launcher
  does), so the service proves it stopped and moves to the next one; a
  supervisor that died while the machine was idle does not wedge the
  workspace until eviction. `stop_machine(G)` also takes a live machine of
  a lower generation, as the launcher's stop does.
- **Host-side deadline.** The adapter records each exec's deadline at its
  first dispatch: the dispatch time on the backend's clock (the kernel
  `Runtime` of its egress ports) plus `timeout_ms` plus 30 s; a replay never
  extends it. The guest ends a command at its own deadline, so an observe
  that still reports a non-terminal state after the host's deadline means
  a guest that lies (it shares the workload's uid) and would hold the
  workspace's lease for ever: the adapter sends `cancel` (its answer
  awaited at most 2 s) and marks the machine unreachable as a protocol
  violation, also when its caller stops waiting first, so the service's
  observe fails and the operation becomes uncertain. A terminal state
  reported late is still accepted. Only non-terminal states are covered:
  a receipt reported without `eof` for ever, or a write acked as running,
  is not cut off yet. The clock is wall time, so a large forward step of
  the host clock can cut off an honest exec (it then ends uncertain).
- **Usage completeness.** The guest's `usage` is used only when it reports
  `complete: true`; otherwise the launcher's volume sizes (measured by
  the runtime, outside the workload's reach) are used, and when the
  launcher cannot answer either the call fails instead of answering 0,
  because the figure gates the quota check.
- A handle with no live machine answers `Unavailable`, never `NotFound`,
  so a missing machine cannot read as a missing file (the copy-in name
  probe relies on `NotFound`).
- Guest replies are untrusted: the supervisor shares the workload's uid in
  a container. The adapter enforces every bound itself (output per observe,
  entries per listing, bytes per read, path syntax) and never lets a
  terminal state it has seen change. An output reply may advance the
  sequence further than the bytes it carries (a workload that truncates its
  own log shortens reads), but never beyond the requested maximum.

### Tests

- Each crate has unit tests that run on macOS and Linux.
- `agentkit-sandboxd` has Linux-only integration tests run in a container
  of the image.
- The existing backend conformance suite (`agentkit-testkit`) runs
  unchanged against the container adapter, plus a real-backend network
  suite (INV-10, NET-1 to NET-9). Both are `#[ignore]` tests that need a
  launcher socket in the environment, and are run on Docker 28+ and
  rootless Podman on Ubuntu 24.04 and Debian 13 VMs.

## Consequences

- `check-arch` gains five `LAYERS` entries; `CONTEXT_DEPS` is unchanged.
- Image v1 adds `agentkit-sandboxd` (static musl, built for amd64 and
  arm64) as its entrypoint and the proxy settings to
  `/etc/sandbox/environment`. v1 was built from scratch and reproduced by
  `build.sh verify` on 2026-11-26 (index `sha256:78e813b3…`,
  `sandbox-image/lock/image-digests.txt`) and qualified on that digest on
  amd64 Docker and rootless Podman (WP-2.1 evidence:
  `docs/evidence/sandbox-m2/wp-2.1-conformance.md`); arm64 has passed the
  image self-test only.
- Programs that need raw TCP do not work on the server (spec R-11).
- A supervisor killed by the workload ends the machine; the outcome of a
  running operation is then uncertain, which the service already handles.
- **Rootless Podman (S4).** There the launcher runs as the server's user:
  the server can write `<sock>`, the state directory and the runtime
  socket itself, so the fixed template, the generation records and the
  directory checks hold only against an honest server. They still protect
  against other host users and the workload. Docker deployments separate
  the two (the launcher is root, the server only reaches the launcher's
  socket).

## Not decided

- Hard per-volume quotas per storage driver (WP-2.8 quota matrix).
- The macOS helper's use of the same protocol over vsock (Milestone 3).
