# WP-2.1 launcher live run (Track B, Linux server)

`agentkit-sandbox-launcher` (ADR 0039, "Launcher") was run on 2026-11-25 against
the real runtimes on the probe VMs of `wp-2.1-runtime-probes.md`. It ran against
rootful Docker and rootless Podman on VM 204, and against rootless Podman on
VM 205. Only one VM ran at a time.

The helpers are outside git in `$SPIKE/guest/launcher-live/`, where `$SPIKE` is
`/Users/andrejvysny/workspace/onemind-workspace/spikes/m2-probes`:

- `lk.py`: a frame client that sends one launcher request.
- `check.py`: summarizes `inspect`.
- `inside.sh`: checks run inside the container.
- `mkimage.sh` and `agentkit-sandboxd`: build the test image.

## Setup

- **Binary.** `cargo build --release --locked -p agentkit-sandbox-launcher`
  inside `rust:1.97.0-alpine` on VM 204, from an rsync of `rust/` (without
  `target/`). The result is a static-pie musl binary of 1.6 MB.
  `cargo test --locked -p agentkit-sandbox-launcher` passed in the same
  container, as root only: 52 tests. The non-root paths were unit-tested on
  macOS only.
- **Two builds.**
  - Build 1 ran the full Docker lifecycle and the Docker template
    inspection. It predates the state-directory lock (see "Found and fixed").
  - Build 2 adds only that lock in `state.rs`. It ran the short Docker
    sequence (second-instance refusal, ensure, unconfigured tier, usage,
    stop, destroy) and both Podman runs.
  - After the live runs, `ensure` was changed to record `created_through`
    just before the create request instead of before the directory and
    volume setup. That change is unit-tested only.
- **Image.** The current image has no supervisor yet, so a throwaway image
  `localhost/aks-live:1` was committed from `localhost/agentkit-sandbox:v0`.
  It adds a shell script at `/usr/local/bin/agentkit-sandboxd` that logs its
  arguments and environment, then sleeps and exits on TERM. Its own `Cmd` is
  `["true"]`, so an image command added to the entrypoint would show up as an
  argument.
- **Image references.** Docker used `localhost/aks-live@<image Id>` (equal to
  its `RepoDigests` digest). Podman used `localhost/aks-live@<.Digest>` (its
  `RepoDigests` digest). Each runtime had its own config.
- **Docker.** The version is 29.10.1 per `wp-2.1-runtime-probes.md`; it
  was not re-read in this run. The launcher ran as root; the server uid/gid was `ops`
  (1000). Paths: socket host dir `/run/aks-live/sockets` (root, 0755), listen
  socket mode 0660 with group `ops`, state in `/run/aks-live/state`. The
  client ran as `ops`.
- **Podman 4.9.3 (Ubuntu) and 5.4.2 (Debian).** Rootless: the launcher, the
  server uid and the client were all `tester` (1001). Socket host dir:
  `/run/user/1001/aks-live/sockets`. Listen socket mode 0600.
- **Tmpfs sizes.** 64 MiB for `/tmp` and 32 MiB for `/var/tmp`. The tier was
  the default: 2 CPUs, 2 GiB, 512 pids.

## Template as applied

The table shows `inspect`, plus checks run inside the container as uid 1000.
"Same" means the same as the column to its left.

| | Docker 29 | Podman 4.9 | Podman 5.4 |
|---|---|---|---|
| `NetworkMode`, interfaces | `none`, `lo` only | same | same |
| `CapDrop` | `["ALL"]` | 11 default caps listed by name | not captured |
| `CapEff` / `CapBnd` / `NoNewPrivs` / `Seccomp` | 0 / 0 / 1 / 2 | same | same |
| `ReadonlyRootfs`; write to `/` and to `/run/agentkit` | true; both `EROFS` | same | same |
| `Tmpfs` | as sent, `noexec` | as sent, plus `rprivate,tmpcopyup` | same as 4.9 |
| `/tmp`, `/var/tmp` | 64M, 32M | same, mode 1777, writable by 1000 | not captured |
| `PidsLimit`, `Memory`, `MemorySwap`, `NanoCpus` | 512, 2 GiB, 2 GiB, 2e9 | same | same |
| cgroup `pids.max` / `memory.max` / `memory.swap.max` / `cpu.max` | 512 / 2147483648 / 0 / `200000 100000` | same | same |
| `Init` | false | `null` | `null` |
| `RestartPolicy`, `AutoRemove`, `Privileged`, `CapAdd`, `Devices` | `no`, false, false, null, null | `no`, false, false, `[]`, `[]` | same as 4.9 |
| `Entrypoint`, `Cmd`, supervisor argv | `[sandboxd]`, `[]`, argc 0 | `sandboxd`, `null`, argc 0 | `[sandboxd]`, `null`, argc 0 |
| `User`, `WorkingDir`, PID 1 | `1000:1000`, `/workspace`, the supervisor | same | same |
| `AGENTKIT_*` environment | generation, digest, both socket paths | same | same (from the supervisor's log) |
| `.Config.Image` | the reference as sent | `localhost/aks-live:1` (the tag) | same as 4.9 |
| mounts | `aks-ws-<ws>` RW, `aks-home-<ws>` RW, bind `<host>/<ws>/g<gen>` RO | same | not captured (the bind is read-only: `EROFS`) |
| `/workspace`, `/home/agent` | `agent:agent` 0755, writable | same | same |
| `<host>/<ws>` and `<host>/<ws>/g<gen>` on the host | `root:root` 0711 and `ops:ops` 0711 | `tester:tester` 0711 (both) | not captured |

- The image's `Cmd` was not appended on any runtime: the supervisor got no
  arguments.
- Podman's `inspect` shows the image's tag in `.Config.Image`, not the digest
  reference the launcher sent. The probes saw the reference echoed back. The
  launcher checks a running container's image by its
  `agentkit.sandbox.image` label, so it does not depend on either.
- `create` returned no warnings on any runtime.

## Lifecycle

Docker and Podman 4.9 ran the whole sequence below, and Podman 5.4 a
shorter one. The bad-request row ran on Docker only, and its tier case on
Docker build 2. Every reply matched the design.

| Step | Reply |
|---|---|
| `ensure g1`, twice | `ensured`, the same id; one container |
| `inspect` | `running: 1` |
| `stop g1` (grace 2 s) | `proven: true` after 1.1 s (Docker) / 0.8 s (Podman); `ps -a` empty; `g1` directory removed |
| `ensure g1` | `stale_generation`, current 1 |
| `ensure g2`, then `destroy` / `reset_home` | `ensured`; then `busy`, `busy` |
| `kill` g2, then `ensure g2` | `busy`: "generation 2 exists (exited) and cannot start again" (not restarted) |
| `stop g2` | `proven: true`, the exited container removed |
| `stop g3` (never started) | `proven: true`; then `ensure g3` is `stale_generation`, current 3 |
| launcher restart, `ensure g2` | `stale_generation`, current 3 (state reloaded; a clean restart on Podman 4.9, on Docker see below) |
| `ensure g4`, launcher restart, `inspect` (Docker only) | `running: 4`, `stopped_through: 3`; startup log "1 managed containers (1 running)" |
| `ensure g5`, `rm -f` it outside the launcher, `ensure g5` | `busy`: "generation 5 had a container already and cannot start again" |
| 5 MiB in `/workspace`, 4 KiB in `/home/agent`, `usage` | `workspace_bytes 5242880`, `home_bytes 4096` (from `/system/df`) |
| `reset_home` | `done`; `/workspace` files kept, home volume empty |
| bad id `../etc` / other digest / unconfigured tier | `invalid_request` / `image_mismatch` / `invalid_request` |
| `destroy` | `done`; no `aks-*` volumes, no socket directory left |
| `ensure g5` after `destroy` | `stale_generation`, current 5 (the fence survives `destroy`) |

## Found and fixed during the run

- **A second launcher on the same state directory.** The first Docker
  restart used `pkill -x agentkit-sandbox-launcher`. procps matches only the
  15-character `comm`, so it matched nothing, and three launchers ended up
  running. Each new one removed the old one's listen socket and bound its
  own. Each launcher also keeps its fences in memory, so one could overwrite
  another's records in the shared state file. The launcher now holds an
  exclusive `flock` on `<state_dir>/launcher.lock` for its whole life. A
  second instance exits 2 with "another launcher holds this state
  directory" before it binds anything. This was rechecked live on Docker,
  and a unit test covers it. Docker's restart rows above came from those
  runs: the new instance answered, read the state file afresh, while the
  old ones were still alive. Podman's restart was a clean one.

## Caveats

- **Placeholder supervisor.** The supervisor was a placeholder, so these
  were not exercised:
  - the control and egress sockets;
  - the handshake;
  - a supervisor that dials and exits.

  Stop latency reflects a TERM trap that exits within 1 s.
- **Not run.** Debian's Docker and the second rootless user were not run.
- **Server user.** Docker's server user was the login user `ops` (uid 1000),
  which the probes already flag as a deployment hazard.
- **Cleanup.** Nothing is left on either VM: no containers, volumes, test
  images or launcher files, and the cargo cache volume and rsync copy are
  gone. Both VMs are shut down.
