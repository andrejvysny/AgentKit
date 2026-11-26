# WP-2.1 real-runtime conformance (Track B, Linux server)

Run on 2026-11-25 on the probe VMs of `wp-2.1-runtime-probes.md`, one VM at a
time. The real launcher (`agentkit-sandbox-launcher`), the real supervisor
(`agentkit-sandboxd`, PID 1 of image v1) and the container adapter
(`agentkit-sandbox-container`) ran against rootful Docker and rootless Podman
on both VMs. Scripts, configs, binaries and raw logs are outside git in
`$SPIKE = /Users/andrejvysny/workspace/onemind-workspace/spikes/m2-probes`:
`$SPIKE/guest/conformance/` (build and run scripts, launcher configs, the
scratch manual-check test), `$SPIKE/guest/image-v1/` (the qualification
image), `$SPIKE/artifacts/wp-2.1-v1/` (the binaries and image archive that
ran), `$SPIKE/logs/conformance/` (logs, named `<host>-<runtime>-*.log`).

The sections up to "Open items" record that run, on the provisional image,
as history. The rerun after the security fixes, on the final v1 image, is
the last section, "After the security fixes".

## Verdict

- **The backend conformance suite (29 cases) and the network suite (7 tests:
  INV-10, NET-1 to NET-8) pass on all four runtimes**: Docker 29.10.1 and
  rootless Podman 4.9.3 on Ubuntu 24.04 (VM 204), Docker 29.10.1 and
  rootless Podman 5.4.2 on Debian 13 (VM 205).
- The manual checks on a live machine (below) hold on all four runtimes.
- No bug was found in the crates; no crate was changed.
- Two parts of the brief could not run inside the approved downloads:
  - **The image is a provisional v1.** `sandbox-image/` carries the v1
    changes, but a from-scratch `build.sh verify` needs BuildKit and the
    Debian base from Docker Hub, Node from nodejs.org and wheels from PyPI.
    The runs used an equivalent overlay: v0 by index digest plus exactly the
    v1 file changes, built offline. Its digests are not the final v1 digests.
  - **Linux clippy did not run.** `rust:1.97.0-alpine` has no clippy
    component, and installing it needs static.rust-lang.org. Clippy ran on
    macOS; Linux ran `cargo check --all-targets` and the test build with
    warnings denied.

## Hosts and runtimes

| | VM 204 `m2-ubuntu2404` | VM 205 `m2-debian13` |
|---|---|---|
| OS / kernel | Ubuntu 24.04.5 LTS, 6.8.0-151-generic | Debian 13, 6.12.118+deb13-cloud-amd64 |
| Docker | 29.10.1, API 1.57, containerd v2.4.2, runc 1.5.3, overlayfs with the containerd snapshotter, cgroup v2 | same |
| Podman (rootless, `tester` uid 1001) | 4.9.3, compat API 1.41, runc 1.5.3, cgroup v2, delegated `cpu memory pids` | 5.4.2, compat API 1.41, runc 1.5.3 |
| Volume filesystem | not re-read | ext4 root (`/var/lib/docker/volumes` and Podman's volume store), no `prjquota` |

Logs: `ubuntu-00-hostfacts.log`, `debian-00-hostfacts.log`. Both VMs: 4 vCPU,
8 GiB, no swap (probes report).

## Builds

All Linux builds ran on VM 204 in `rust:1.97.0-alpine@sha256:ec9c91e7…`
(rustc 1.97.0, host `x86_64-unknown-linux-musl`) from an rsync of `rust/`
(the working tree on 2026-11-25, `Cargo.lock` sha256 `34de69d6…`; no source
file under `rust/` changed between the rsync and this report).

- `cargo fetch --locked` was the only step with network (crates.io). Every
  other step ran with `--network none` and `--offline`, with
  `RUSTUP_TOOLCHAIN=1.97.0-x86_64-unknown-linux-musl` so rustup did not try
  to add the components `rust-toolchain.toml` lists. Script:
  `$SPIKE/guest/conformance/build-linux.sh`.
- Packages: `-p agentkit-sys -p agentkit-sandboxd -p agentkit-sandbox-launcher
  -p agentkit-sandbox-container -p agentkit-egress -p agentkit-sandbox-proto`.

| Step (Linux, `RUSTFLAGS=-Dwarnings`) | Result |
|---|---|
| `cargo check --locked --offline <pkgs> --all-targets` | ok (`ubuntu-01-check.log`) |
| `cargo test --locked --offline <pkgs> --no-run` | ok, 25 test binaries |
| `cargo test --locked --offline <pkgs> --doc` | ok (no doc tests) |
| `cargo build --release -p agentkit-sandbox-launcher` | ok, static-pie, sha256 `30d4abcc…` |
| `cargo clippy … --all-targets -- -D warnings` | **not run** on Linux (no clippy component in the image) |
| same clippy command on macOS (aarch64-apple-darwin) | ok, no warnings |

- **Tests ran on the VM host, not in the build container.** The alpine
  image has no `/bin/bash`, which the supervisor and adapter tests start. The
  binaries are static musl, so each ran directly on Ubuntu with its package
  directory as working directory, as `cargo test` does
  (`$SPIKE/guest/conformance/run-tests.py`). As `ops`: 276 passed, 0 failed,
  9 ignored (the real suites and the in-process network case). This includes
  `sandboxd`'s Supervisor-mode test (`supervisor_mode`: reap-all path,
  subreaper, background job ended, cancel, shutdown) and the Linux-only
  `prctl` and `/proc` paths of `agentkit-sys`.

  | Crate | Passed |
  |---|---|
  | agentkit-sys | 34 |
  | agentkit-sandbox-proto | 33 |
  | agentkit-sandboxd | 80 |
  | agentkit-egress | 49 |
  | agentkit-sandbox-launcher | 52 |
  | agentkit-sandbox-container | 28 (+9 ignored) |

- As root, the same 276 ran with one failure:
  `agentkit-sys process::tests::own_parent_pid_and_effective_uid_match_the_platform`.
  The test asserts that the working directory belongs to the effective uid,
  and the checkout belonged to `ops`. It is an environment assumption of the
  test, not a crate fault (`ubuntu-03-tests-root.log`).
- The in-process conformance run with real egress
  (`conformance --ignored`: 29 cases on the adapter with an embedded
  supervisor, reaching `https://example.com`) passed in 11.3 s.
- **Supervisor binary.** `./build.sh sandboxd` (new, below) built
  `agentkit-sandboxd` twice from clean target directories: both times
  sha256 `f22918bd614cabeb61d22553a0133e7f87ee3e234b123e3fc8bfad9d17a8383a`,
  1 352 480 bytes, static-pie, stripped (`ubuntu-05-sandboxd-build.log`).
  `manifest.env` pins this sha256.
- The test binaries (`real_backend`, `real_network`, the scratch
  `manual_hold`), the launcher and the image archive were built once on VM 204
  and copied to VM 205 unchanged (sha256s in `$SPIKE/artifacts/wp-2.1-v1/`).

## Image v1

### What changed in `sandbox-image/`

- `/usr/local/bin/agentkit-sandboxd` (root, 0755), checked against
  `SANDBOXD_SHA256_AMD64` in the Dockerfile; `ENTRYPOINT
  ["/usr/local/bin/agentkit-sandboxd"]`, no `CMD`; label version `v1`.
- `/etc/sandbox/environment`: `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`,
  `https_proxy` = `http://127.0.0.1:3128`, `NO_PROXY`/`no_proxy` =
  `localhost,127.0.0.1,::1`. npm's builtin npmrc adds `proxy` and
  `https-proxy`. pip needs no setting of its own: it honours `HTTPS_PROXY`
  (manual check "pip, env only").
- `manifest.env`: `IMAGE_VERSION=v1`, `PLATFORMS=linux/amd64`, `RUST_IMAGE`
  by digest, `SANDBOXD_SHA256_AMD64`.
- `build.sh`: `sandboxd` (build the supervisor in `RUST_IMAGE`, fetch then
  offline build, fixed `/src` and `/target` paths); `finalize` (run by
  `build`: wrap a one-platform build in an index and name it
  `localhost/agentkit-sandbox:<version>` in `index.json` with
  `io.containerd.image.name` and `org.opencontainers.image.ref.name`);
  `--entrypoint` in `selftest` and `notice`; the platform list from
  `PLATFORMS`. `.dockerignore` lets `out/agentkit-sandboxd-*` in.
- **amd64 only.** The supervisor is built in the amd64 Rust image, and an
  aarch64 musl build would need a rustup target that was not approved.
- **Not regenerated:** `lock/image-digests.txt`, `lock/dpkg-linux-*.txt`
  and `notice/` are still v0's. `build`, `verify`, `load` and `notice` did
  not run. `finalize` and `sandboxd` ran on VM 204; the selftest ran by
  hand with `selftest`'s new `--entrypoint` form (below). The v1
  Dockerfile itself has not been built: the `.dockerignore` re-include, the
  `${TARGETARCH}` COPY and its sha256 check, and the CMD reset by
  `ENTRYPOINT` are exercised only through their copies in the overlay.

### The qualification image (provisional)

`$SPIKE/guest/image-v1/Dockerfile`: `FROM
localhost/agentkit-sandbox@sha256:98473624…` (v0), then the same
environment file, the same supervisor binary with the same sha256 check,
the same npmrc line, the setuid and uid-1000 checks, the v1 label and the
entrypoint. It was built on VM 204 with the default `docker` driver
(BuildKit v0.35.1), `--network none`, `rewrite-timestamp=true` and
`SOURCE_DATE_EPOCH` from the manifest, then run through `build.sh
finalize` (`ubuntu-06-overlay-build.log`, `ubuntu-07-overlay-digests.log`).

| | Digest |
|---|---|
| index (the reference) | `sha256:e2c39b769c3a83c135af3b58e4541b18bcc0243c3cdc469271dcbf7e50e8e076` |
| linux/amd64 manifest | `sha256:fb443132f1b32e5fc855e34e075955ed36aa6077d04edbd212d8ca8fb7f37c4a` |
| config | `sha256:b82c2be10119eaa0f9c5d82fb28c5a1b3e2b81125e4e33ede64e05a1cb0720b2` |
| archive `image-v1.oci.tar` | sha256 `8429e035…`, 226 457 600 bytes |

- Layers: v0's 13 amd64 layers unchanged, plus three: the environment file
  (737 B), the supervisor (648 808 B compressed) and the npmrc (292 B).
  Config: `Entrypoint` the supervisor, `Cmd` null, `User 1000:1000`,
  `WorkingDir /workspace`.
- The final v1 from `build.sh verify` lays these changes into the
  Dockerfile's own layers and will have other digests. Its runs are an open
  item.
- With `--platform linux/amd64`, the docker driver resolved `FROM` at the
  registry (`https://localhost/v2/…`, refused) instead of the local image
  store; without it, the local v0 was used. Nothing was pulled.

### Load and reference (both VMs)

| | Docker 29 | Podman 4.9 / 5.4 |
|---|---|---|
| `load -i image-v1.oci.tar` | `Loaded image: localhost/agentkit-sandbox:v1` | `Loaded image: localhost/agentkit-sandbox:v1` |
| image `Id` | the index digest | the config digest |
| `RepoDigests` | `@<index>` | `@<index>`, `@<manifest>` |
| API `POST /containers/create` with `localhost/agentkit-sandbox@<index>` | 201 | 201 |

The `index.json` annotations named the image on both runtimes, so no manual
tag was needed (the probes had to tag). Logs: `*-08-load-*.log`, the
`probe` step in each run log.

`sandbox-selftest` on the qualification image under Docker on VM 204
(`--init --network none --read-only --cap-drop ALL`, user 1000, fresh
volumes, `--entrypoint sandbox-selftest`): 21 of 21 pass, including the
uid-1000 and setuid scans (`ubuntu-09-selftest-docker.log`).

## Deployment

| | Docker (both VMs) | Podman (both VMs) |
|---|---|---|
| Launcher runs as | root, `systemd-run --unit aks-launcher` | `tester`, `systemd-run --user --unit aks-launcher` |
| Runtime socket | `/run/docker.sock` | `/run/user/1001/podman/podman.sock` |
| Listen socket | `/run/agentkit-launcher/launcher.sock`, 0660, group `aksrv` | `/run/user/1001/agentkit/launcher.sock`, 0600 |
| `socket_host_dir` | `/srv/agentkit/sockets` (root, 0755) | `/run/user/1001/agentkit/sockets` (tester, 0755) |
| Server uid/gid (owns `g<gen>`, binds the sockets, runs the tests) | `aksrv` 1002 | `tester` 1001 |
| State | `/var/lib/agentkit-launcher` (0700) | `~tester/.local/state/agentkit-launcher` |
| Tier, tmpfs | defaults: 2 CPUs, 2 GiB, 512 pids; `/tmp` 256 MiB, `/var/tmp` 64 MiB | same |

- Configs: `$SPIKE/guest/conformance/launcher-docker.json`,
  `launcher-podman-<host>.json`; image reference
  `localhost/agentkit-sandbox@sha256:e2c39b76…`.
- `aksrv` (uid/gid 1002) was created on both VMs for this run, so the server
  is not uid 1000. Container uid 1000 still maps to host uid 1000 (`ops`)
  under Docker, which owns the volume files on the host (probes hazard,
  unchanged).

## Commands

Per runtime (`$SPIKE/guest/conformance/run-suite.sh <docker|podman> probe
launcher-start backend network manual manual-cleanup leftovers
launcher-stop`), from `crates/sandbox-container` as the server user:

```
AGENTKIT_SANDBOX_LAUNCHER_SOCKET=<listen socket> \
AGENTKIT_SANDBOX_SOCKET_DIR=<socket_host_dir> \
AGENTKIT_SANDBOX_IMAGE_DIGEST=sha256:e2c39b769c3a83c135af3b58e4541b18bcc0243c3cdc469271dcbf7e50e8e076 \
AGENTKIT_SANDBOX_PRIVATE_NAME=aks-net3.internal \
  real_backend --ignored --nocapture      # then real_network --ignored --nocapture
```

`real_backend` and `real_network` are the binaries of `cargo test --locked
-p agentkit-sandbox-container --test real_backend|real_network --no-run`
from the alpine build; running them directly is the run step of `cargo
test … -- --ignored --nocapture`. For NET-3 each VM had a temporary
`/etc/hosts` line `10.255.255.1 aks-net3.internal` (removed afterwards).

## Results

### Backend conformance suite (`real_backend`)

The suite runs its cases in order on fresh workspaces and stops at the
first failure, naming it; every run finished with no failure.

| Case | Docker 204 | Podman 4.9 (204) | Docker 205 | Podman 5.4 (205) |
|---|---|---|---|---|
| describe_is_stable | PASS | PASS | PASS | PASS |
| ensure_machine_is_idempotent | PASS | PASS | PASS | PASS |
| stale_generation_is_refused_everywhere | PASS | PASS | PASS | PASS |
| stopped_generation_does_not_restart | PASS | PASS | PASS | PASS |
| proven_generations_never_start | PASS | PASS | PASS | PASS |
| stop_proves_stopped_then_next_generation_starts | PASS | PASS | PASS | PASS |
| operations_belong_to_their_generation | PASS | PASS | PASS | PASS |
| home_and_storage_refused_while_running | PASS | PASS | PASS | PASS |
| exit_status_and_output_are_reported | PASS | PASS | PASS | PASS |
| same_id_and_args_is_idempotent | PASS | PASS | PASS | PASS |
| same_id_other_args_conflicts | PASS | PASS | PASS | PASS |
| unknown_operation_is_unknown | PASS | PASS | PASS | PASS |
| terminal_receipt_is_stable | PASS | PASS | PASS | PASS |
| observe_never_blocks_on_a_running_operation | PASS | PASS | PASS | PASS |
| cancel_ends_the_operation_as_cancelled | PASS | PASS | PASS | PASS |
| deadline_ends_the_operation_as_timed_out | PASS | PASS | PASS | PASS |
| endless_output_is_bounded | PASS | PASS | PASS | PASS |
| background_process_ends_with_the_command | PASS | PASS | PASS | PASS |
| write_then_read_round_trips | PASS | PASS | PASS | PASS |
| write_with_wrong_digest_changes_nothing | PASS | PASS | PASS | PASS |
| read_refuses_missing_directories_and_large_files | PASS | PASS | PASS | PASS |
| list_honours_depth_and_limit | PASS | PASS | PASS | PASS |
| reads_run_beside_a_running_operation | PASS | PASS | PASS | PASS |
| files_persist_across_stop_and_new_generation | PASS | PASS | PASS | PASS |
| storage_usage_counts_written_bytes | PASS | PASS | PASS | PASS |
| home_is_a_separate_tree_emptied_by_reset_home | PASS | PASS | PASS | PASS |
| raw_read_returns_binary_bytes | PASS | PASS | PASS | PASS |
| network_denied_without_grant_is_drained | PASS | PASS | PASS | PASS |
| network_policy_applies_live (curl without `--proxy`: the image's proxy variables) | PASS | PASS | PASS | PASS |
| whole suite | 35.1 s | 31.9 s | 29.7 s | 30.4 s |

### Network suite (`real_network`)

| Test | Covers | Docker 204 | Podman 4.9 (204) | Docker 205 | Podman 5.4 (205) |
|---|---|---|---|---|---|
| inv10_nothing_leaves_without_a_grant | INV-10, NET-1, NET-5 | PASS | PASS | PASS | PASS |
| net2_a_grant_opens_exactly_its_host_and_port | NET-2, NET-4 | PASS | PASS | PASS | PASS |
| net3_a_granted_name_on_a_private_address_is_refused | NET-3 | PASS | PASS | PASS | PASS |
| pip_downloads_through_the_package_grants | default package grants | PASS | PASS | PASS | PASS |
| net6_a_revoke_closes_an_open_tunnel_within_five_seconds | NET-6 | PASS | PASS | PASS | PASS |
| net7_the_guest_reaches_no_other_socket | NET-7 | PASS | PASS | PASS | PASS |
| net8_the_tunnel_limit_holds | NET-8 | PASS | PASS | PASS | PASS |
| whole suite (tests in parallel) | | 18.6 s | 18.7 s | 18.0 s | 19.0 s |

NET-9 (no content in the proxy's logs) is not visible from inside the
guest; the egress crate's tests cover it. The tests reached example.com,
pypi.org and files.pythonhosted.org through grants; 1.1.1.1 was only
probed and refused.

### Manual checks on a live machine

A scratch test (`$SPIKE/guest/conformance/manual_hold.rs`, compiled into
`crates/sandbox-container/tests/` on the VM copy only) started one machine
with grants for pypi.org and files.pythonhosted.org, ran each check as an
operation (so as the workload, uid 1000), then held the control channel
open while the host was inspected, and was killed with SIGKILL. Logs:
`<host>-<runtime>-manual.log`.

| Check | Docker 204 | Podman 4.9 (204) | Docker 205 | Podman 5.4 (205) |
|---|---|---|---|---|
| PID 1 is `agentkit-sandboxd`, uid/gid 1000, CapEff/CapBnd 0, NoNewPrivs 1, Seccomp 2 | yes | yes | yes | yes |
| `/proc/1/environ` and `/proc/1/mem` owned by root, `cat /proc/1/environ` denied, `/proc/1/fd` unlistable (non-dumpable) | yes | yes | yes | yes |
| `ptrace(PTRACE_ATTACH, 1)` from the workload | EPERM | EPERM | EPERM | EPERM |
| On the host, the supervisor's `/proc/<pid>/environ` owner | 0:0 (host pid uid 1000) | 1001:1001 = userns root (host pid uid 166535) | 0:0 | 1001:1001 |
| `sleep 300 &` then exit: processes at the next operation | PID 1 + that operation's shell | same | same | same |
| `setsid -f sleep 301` escapee: visible during the op (ppid 1), gone after | yes | yes | yes | yes |
| Orphan that exits (reparented to PID 1): no zombie 2 s later, none seen from the host while held | yes | yes | yes | yes |
| `/home/agent` of a fresh workspace | empty | empty | empty | empty |
| Command environment has the six proxy variables | yes | yes | yes | yes |
| pip, env only (`pip download six`, no `--proxy`) | downloaded | downloaded | downloaded | downloaded |
| npm `proxy` / `https-proxy` | the shim | the shim | the shim | the shim |
| npm without a grant (`npm view left-pad`) | 403 from the proxy; denial `registry.npmjs.org:443` drained with its operation id | same | same | same |
| Channel loss (host process SIGKILL) to container not running | 0.18 s, exit 1 | 0.07 s, exit 1 | 0.16 s, exit 1 | 0.07 s, exit 1 |
| Supervisor log after the loss | `connected`, `serving generation 1`, `control channel lost` | same | same | same |

`/workspace` of the fresh workspace held one entry at the first check:
`.sandbox`, the supervisor's log directory made by that check itself. The
zombie check shows that no zombie is left; it does not catch one that lives
for less than the 2 s window.

## Bugs found and fixed

None in the crates; no `.rs` or `Cargo.toml` file was changed, so no API
baseline changed. Two problems were in this run's own tooling and were
fixed there: an rsync `--exclude target/` that also dropped
`crates/egress/src/target/` (anchored to `/target/`), and the
`--platform` registry lookup above.

## Deviations

- **Provisional image.** The runs used the offline overlay described above,
  not a `build.sh` build, because the full build needs downloads outside the
  approved list. Its digests are recorded as provisional.
- **amd64 only** in v1 (manifest and README say so).
- **Linux clippy not run**; Linux `cargo check --all-targets` and the test
  build with `-Dwarnings`, and clippy on macOS, instead.
- **`cargo test` split**: built in `rust:1.97.0-alpine`, run on the VM host
  (no bash in the alpine image; host sockets and uids are simpler). Doc tests
  ran in the container. The real suites ran on VM 205 from binaries built
  on VM 204.
- **pip**: no pip config was added; the environment's `HTTPS_PROXY` is
  enough (checked live).

## Quota

No hard per-volume quota on these hosts: the volumes live on the ext4 root
without project quotas (checked on VM 205; VM 204 has the same cloud-image
layout, not re-read). The adapter's `describe()` reports
`enforces_disk_quota: false` (`sandbox-container/src/port.rs`), so the soft
check of ADR 0039 applies: the service checks usage before each mutating
operation. The usage it reads was right on every runtime
(`storage_usage_counts_written_bytes` passed); the service-side check
itself is not part of these suites.

## Cleanup

On both VMs nothing of the run is left: no containers, no `aks-*` or cargo
volumes, no BuildKit cache, no launcher unit or process, no state or socket
directories, no `/srv/aks` build tree, and no `/etc/hosts` line
(`*-10-cleanup.log`). Kept: the loaded images (Docker: v0,
`rust:1.97.0-alpine` and `localhost/agentkit-sandbox:v1`; `tester`'s Podman:
v0 and `localhost/agentkit-sandbox:v1`). That `v1` is the provisional
qualification image `sha256:e2c39b76…`, not a `build.sh` v1. Also kept: the
`aksrv` account (skeleton home only). Both VMs are shut down.

## Open items

- Run `./build.sh sandboxd`, `verify`, `load`, `selftest` and `notice` with
  the network access the README lists, commit the regenerated `lock/` and
  `notice/`, then rerun both suites on the final v1 index digest.
- arm64: a pinned aarch64 musl build of the supervisor.
- Clippy on Linux (an image or toolchain with the clippy component).
- The NOTICE does not list the Rust crates linked into `agentkit-sandboxd`.
- `agentkit-sys` test `own_parent_pid_and_effective_uid_match_the_platform`
  assumes the checkout belongs to the user running the tests.

## After the security fixes

Rerun on 2026-11-26 after the security fixes of ADR 0039 (flat socket
layout `<socket_host_dir>/<machine_dir_name>`, socket host directory 0710
with the server's group under Docker and 0700 under rootless Podman,
checked ancestors, a per-generation channel token, `max_containers`, a stop
of G covering every container at or below G). Same VMs, one at a time.
Scripts, configs and the scratch manual-check test are in
`$SPIKE/guest/after-security/` (`gate-mac.sh`, `run-tests.py`,
`run-after.sh`, `manual_security.rs`, `launcher-*.json`, `lk.py`); logs in
`$SPIKE/logs/after-security-fixes/` (`mac-*` for the Mac, `ubuntu-*` and
`debian-*` for the VMs, `<host>-<runtime>-*` for the launcher and test
output). The earlier logs were left as they were.

Hosts as in the first run: Ubuntu 24.04.5 LTS (6.8.0-151-generic) and
Debian 13 (6.12.118+deb13-cloud-amd64); Docker 29.10.1 (API 1.57,
containerd snapshotter, containerd commit `b7a25261`, runc 1.5.3); rootless
Podman 4.9.3 and 5.4.2 (runc 1.5.3); cgroup v2 (`ubuntu-00-hostfacts.log`,
`debian-00-hostfacts.log`).

### Verdict

- **Image v1 is final, for amd64 and arm64.** `build.sh` built it from
  scratch on the Mac and `verify` reproduced it byte for byte (two builds,
  the second cold, plus a third cold build). Index
  `sha256:78e813b333317aef6ad8bdc4cf3682b9752b05f54f88eb8051535dd27a07de93`.
  `sandbox-selftest`: 21 of 21 on both platforms.
- **Linux gate:** clippy with `-D warnings` is clean for the seven crates;
  381 tests pass and none fail on both VMs (as `ops`, umask 0002), after a
  fix to the launcher's test helpers (below). The Linux-only tests ran.
- **Both suites pass on all four runtimes on the final index digest**
  (Docker 29.10.1 and rootless Podman 4.9.3 on VM 204, Docker 29.10.1 and
  rootless Podman 5.4.2 on VM 205).
- **The manual security checks hold on all four runtimes.**
- Four problems were fixed, none in a crate's non-test code: the image
  build's .pyc reproducibility, `build.sh sandboxd` on Docker's classic
  image store, the launcher's test helpers under umask 002, and
  `real_backend` against `max_containers`. No public API changed.

### Source state

- One tree for every Rust build: `rust/` of the working tree,
  `Cargo.lock` sha256 `34510f22d8bebba5808e4f544d6c57f22763e698e333ea0d57a069181f402dbd`.
  Between the first build and the last VM copy, only this run's two edits
  changed (`sandbox-container/tests`, `sandbox-launcher` test helpers);
  hashes of the 9 crates involved (and of the whole tree) were compared
  before the first build, before the VM 204 and VM 205 copies, and after
  the runs.
- `agentkit-sandboxd` (in the image) was built before both edits; neither
  reaches it. The launcher release binary was the same before and after
  the launcher edit (sha256 `a484f496…`; the edit is `#[cfg(test)]` code).

### Builds (Mac: OrbStack, Docker 29.8.2, BuildKit v0.35.1)

| Step | Result |
|---|---|
| `./build.sh sandboxd` | amd64 `cdcc41ac1a1fd750dfd1e3b159a1ea4363e5416748388a8efdf93fa24f18808e` (static-pie, 1 356 576 B, Rosetta); arm64 `b0a07a6dd36062d32b3da81e643465053e345ccfc22f814e1c59bae9c3237fcf` (static, 1 249 552 B, native); a second build from clean target directories gave the same sha256s (`mac-01`, `mac-02`) |
| `./build.sh build` | index `sha256:9d663a3a…` (with `compileall -j0`; superseded) (`mac-03`) |
| `./build.sh verify` (1st) | **digests differed**: amd64 equal, arm64 not; one .pyc in the compileall layer (`mac-04`, below) |
| `./build.sh verify` (2nd, after the fix) | identical index, manifests and configs; archives byte-identical (`mac-05`) |
| `./build.sh build --no-cache` (3rd, cold) | archive identical to the 2nd verify's (`mac-06`) |
| `./build.sh load`, `selftest` | 21 of 21 on `linux/amd64` and `linux/arm64` (`mac-30`, `mac-31`) |
| `./build.sh notice` | 149 Debian packages on each platform; the NOTICE's new last section lists 35 entries for `agentkit-sandboxd` (`mac-32`) |

| | Digest |
|---|---|
| index (the reference) | `sha256:78e813b333317aef6ad8bdc4cf3682b9752b05f54f88eb8051535dd27a07de93` |
| linux/amd64 manifest / config | `sha256:c0eec9f411447d46d6b38ffba549464234a31679f6a21cabe5ca0f8752973959` / `sha256:687fe67511222c7fbb32c7f7230a929cd77038c88ab82169c3501841eae935d9` |
| linux/arm64 manifest / config | `sha256:6fcb57337145e46cf39809c40041793093920ca1e729bc89a958531518dbfa16` / `sha256:2a4258daebe0a62554ef7d23f12089cd4e20e0edfb0dfe61504006a29a308f32` |
| `out/image.oci.tar` | sha256 `f6f21eb6ad3c2735ea747a2bc00fbf8339aac6fc33b6dab8d54cd3b7e66d9a7f`, 452 567 040 B, 15 layers per platform |

Recorded in `sandbox-image/lock/image-digests.txt` (written by `verify`),
the README's "Measured (v1)" and `manifest.env` (`SANDBOXD_SHA256_AMD64`,
`SANDBOXD_SHA256_ARM64`). The arm64 supervisor starts and refuses to run
without its environment (`AGENTKIT_GENERATION is not set`); it did not run
on a launcher, since both VMs are amd64.

### Linux gate

Built on the Mac in `rust:1.97.0-alpine` (amd64 manifest digest
`sha256:c6b95caf…` of the pinned index, under Rosetta) plus the clippy and
rustfmt components (`rustup component add`, saved as a local image for the
run), sources mounted read-only at `/srv/aks/src/rust` (the VM path), crates
fetched first, every other step `--network none --offline`,
`RUSTFLAGS=-Dwarnings`, `CARGO_INCREMENTAL=0`, debug info
`line-tables-only` (`$SPIKE/guest/after-security/gate-mac.sh`). Packages:
`agentkit-sys`, `-sandboxd`, `-sandbox-launcher`, `-sandbox-container`,
`-egress`, `-sandbox-proto`, `-kernel`.

| Step | Result |
|---|---|
| `cargo clippy --locked --offline <pkgs> --all-targets --all-features -- -D warnings` | ok, no warnings (`mac-11`; again after the launcher fix, `mac-21`) |
| `cargo test --locked --offline <pkgs> --no-run` | 30 test binaries (`mac-12`, `mac-22`) |
| `cargo test … --doc` | ok, 7 crates, no doc tests (`mac-23`) |
| `cargo build --release -p agentkit-sandbox-launcher` | static-pie, sha256 `a484f496043e664d24d07ff2bc03b942fb5e54468653d013251c7b40e15a99da` |

The test binaries ran on each VM host as `ops` (the checkout owner; umask
0002), from each package's directory (`run-tests.py`, full output in
`ubuntu-21-tests-ops.log`, `debian-21-tests-ops.log`):

| Crate | VM 204 passed | VM 205 passed |
|---|---|---|
| agentkit-sys | 35 | 35 |
| agentkit-kernel | 17 | 17 |
| agentkit-sandbox-proto | 35 | 35 |
| agentkit-sandboxd | 97 | 97 |
| agentkit-egress | 83 | 83 |
| agentkit-sandbox-launcher | 73 | 73 |
| agentkit-sandbox-container | 41 (+9 ignored) | 41 (+9 ignored) |
| total | 381, 0 failed | 381, 0 failed |

- Linux-only tests that ran and passed: `supervisor_mode::the_reap_all_path_serves_commands_end_to_end`
  (PID 1's reap-all path), `usage::tests::a_name_that_is_not_utf8_is_incomplete`
  and `listing::tests::a_name_that_is_not_utf8_is_left_out_and_marks_the_listing_truncated`
  (non-UTF-8 names, which APFS refuses), and `agentkit-sys`'s
  `supervisor::tests` on `/proc`.
- The 9 ignored are `real_backend` (1), `real_network` (7) and the
  in-process conformance run with internet access (1, not rerun).
- The first run on VM 204 (`ubuntu-20-tests-ops.log`) failed 31 launcher
  unit tests, all with "`/tmp/.tmpXXXXXX` is writable by group or others":
  see fix 3.

### Deployment (the new permission model)

| | Docker (both VMs) | Podman (both VMs) |
|---|---|---|
| Launcher runs as | root, `systemd-run --unit aks-launcher` | `tester`, `systemd-run --user` |
| `socket_host_dir` | `/srv/agentkit/sockets`, `drwx--x--- root:aksrv` (0710) | `/run/user/1001/agentkit/sockets`, `drwx------ tester` (0700) |
| its ancestors | `/srv/agentkit`, `/srv`, `/`: root, 0755 | `/run/user/1001/agentkit` and `/run/user/1001` (tester, 0700), `/run/user`, `/run`, `/`: root, 0755 |
| listen socket | `/run/agentkit-launcher/launcher.sock` 0660 `root:aksrv`, directory root 0755 | `/run/user/1001/agentkit/launcher.sock` 0600, directory tester 0700 |
| state directory | `/var/lib/agentkit-launcher` root 0700 | `~tester/.local/state/agentkit-launcher` 0700 (ancestors tester, not group/other-writable) |
| machine directory / sockets | `drwx--x--x aksrv:aksrv` / `srw-rw-rw- aksrv` | `drwx--x--x tester` / `srw-rw-rw- tester` |
| `max_containers` | 8, set explicitly | 8, set explicitly |
| image | `localhost/agentkit-sandbox@sha256:78e813b3…` (Docker: `Id` = the index digest) | same reference (Podman: `Id` = the amd64 config digest, `RepoDigests` index and manifest) |

`docker load` and `podman load` of the two-platform archive both named it
`localhost/agentkit-sandbox:v1`, and an Engine API create by
`localhost/agentkit-sandbox@<index>` answered 201 on all four runtimes
(the `probe` step). Each launcher started with "0 managed containers"; the
socket host directory had no entry before or after any suite (`perms` and
`leftovers` steps).

### Results

| | Docker 204 | Podman 4.9 (204) | Docker 205 | Podman 5.4 (205) |
|---|---|---|---|---|
| `real_backend` (all 29 conformance cases, in order) | PASS, 23.0 s | PASS, 26.9 s | PASS, 27.4 s | PASS, 28.5 s |
| `real_network`: INV-10, NET-2/4, NET-3, pip, NET-6, NET-7, NET-8 (7 tests) | 7 PASS, 18.0 s | 7 PASS, 18.9 s | 7 PASS, 18.5 s | 7 PASS, 18.3 s |

The suite stops at the first failing case and names it; no run failed. The
case list is the one in the first run's table.

**Container cap.** A conformance case leaves its machine running, and
`real_backend` gave each of the 29 cases a fresh workspace and destroyed
them only at the end, so a launcher at `max_containers` 8 would refuse the
ninth case's create. The suite now stops and destroys the previous case's
workspace before the next case starts (fix 4); the launcher log shows
`stop … g4: stopped proven=true` and `destroy …: Done` before each next
`ensure` (`<host>-<runtime>-launcher.log`). The cap was left at its default,
8, and written into each launcher config. Peak live containers, counted
from each launcher log (`ensured` less proven stops): `real_backend` 1,
`real_network` 5 (7 tests, NET-7 holds two machines; at most 8 by
construction), the manual checks 1, on all four runtimes. The manual
leftover containers carry no `agentkit.sandbox.managed` label, so the cap
does not count them.

### Manual security checks

`manual_security.rs` (scratch, compiled into `crates/sandbox-container/tests/`
of a copy of the tree only) and `run-after.sh` steps `manual`,
`others`, `impostors`, `leftover-stop`. Commands run as operations, so as
the workload (uid 1000). On Podman the other host users were probed from
`ops`' session while `tester`'s run held the machine (`tester` has no sudo).

| Check | Docker 204 | Podman 4.9 (204) | Docker 205 | Podman 5.4 (205) |
|---|---|---|---|---|
| Other host users list `socket_host_dir` | `tester`, `ops` (uid 1000, the container's uid under Docker): EACCES | `ops`, `aksrv`: EACCES (at `/run/user/1001`, 0700, before the socket directory) | `tester`, `ops`: EACCES | `ops`, `aksrv`: EACCES (at `/run/user/1001`) |
| … list the machine directory, connect to `control.sock` and `egress.sock` | EACCES on each (not ENOENT) | EACCES on each | EACCES on each | EACCES on each |
| Impostors connecting first (in-process: one silent, one answering `hello` with a wrong token) | both connected 741 ms after polling began and got the adapter's `hello`; wrong token dropped 0.65 ms after connecting, silent one 196 ms after (at the handshake); machine started (`ensure` 0.94 s), exec ran; a later connect: ECONNREFUSED | same; 293 ms, 0.66 ms, 197 ms; `ensure` 0.49 s | same; 546 ms, 0.64 ms, 193 ms; `ensure` 0.74 s | same; 272 ms, 0.65 ms, 192 ms; `ensure` 0.46 s |
| Token out of the workload's reach | `/proc/1/environ` root 0400, `cat` denied; command environment: 0 `AGENTKIT_*` variables; readable `/proc/*/environ` holding `AGENTKIT_CHANNEL_TOKEN`: 0; the runtime's record (`inspect` `Config.Env`) holds exactly one 32-hex token (counted, value not printed) | same | same | same |
| `rm -rf /workspace/.sandbox && ln -sfn / /workspace/.sandbox` | that op: `Succeeded`, its output lost (its log was in the removed directory); next exec `Succeeded` with its output; `.sandbox` a directory again; `.sandbox.broken-<32 hex> -> /` beside it; nothing appeared at `/logs` | same | same | same |
| Usage with a 50 MB file 131 levels deep (`USAGE_DEPTH_MAX` 128) | fresh 4 096; 10 MB shallow file: 10 010 624 (the guest's figure); then 60 000 436, equal to the launcher's `workspace_bytes` 60 000 436 + `home_bytes` 0 at the same moment: the adapter used the launcher's figure | same figures | same figures | same figures |
| Stop at G = 3 with unmanaged leftovers labelled with the workspace: generation 1, 2, none, `two`, 9 | `stopped proven=true`; only the generation-9 container left; then `ensure` 2: `stale_generation` (current 9); stop 9 proven, nothing left; `destroy` done | same (rerun: the first try's `ps --format {{.Label}}` is Docker-only) | same | same |
| After all steps | no `aks-`/`akl-` container or volume, no entry in `socket_host_dir` | same | same | same |

- The guest counts each regular file at the larger of its apparent and
  allocated size (`sandboxd/src/usage.rs`); 10 010 624 fits that (by
  arithmetic, not inspected: 2 442 blocks of 4 KiB for the 10 MB file plus
  one block for each of two log files). Counted that way
  the deep tree would not come to 60 000 436, the launcher's figure, so the
  deep answer is the launcher's. The adapter falls back on an incomplete
  answer and on a guest error alike; that the guest answered `complete:
  false` is inferred from `usage::tests::nesting_past_the_depth_limit_is_incomplete`
  and from the guest serving every other request of that session.
- "Unlabelled" can only mean "without a generation label" here: the
  launcher's stop lists containers by the workspace label.
- The impostors connected as soon as the socket file appeared (1 ms
  polling); that they connected before the supervisor's own dial is not
  observed directly, but both were taken as candidates and greeted, and the
  real guest still won the handshake.
- Logs: `<host>-<runtime>-manual.log`, `-manual-host.log`,
  `-impostors.log`, `*-others.log`, `*-impostors-leftovers.log`. The
  Docker 204 first try (`ubuntu2404-docker-manual-try1.log`) lost part of
  its host-side block to the test's own writes to the same file; the
  host-side output was moved to its own file and the step rerun.

### Fixes

1. **Image build: .pyc not reproducible.**
   `sandbox-image/Dockerfile:96-104`. The first `verify` differed in one
   arm64 file,
   `pandas/tests/groupby/__pycache__/test_api.cpython-313.pyc` (7 223 vs
   7 208 B): one frozenset written with marshal's shared-reference flag in
   one build and without it in the other. The flag follows the object's
   refcount, and with `compileall -j0` a worker's refcounts depend on which
   files it compiled before; the same file compiled alone in a fresh
   process gave one result under six fixed and four random hash seeds. Fix:
   `PYTHONHASHSEED=0 python3 -m compileall -j1`. Test: `verify` (two
   builds, the second cold) and a third cold build gave identical archives;
   in the built images, deleting every `__pycache__` and rerunning the
   Dockerfile's line reproduced all 4 003 shipped .pyc files 8 times on
   arm64 and 3 times on amd64 (`mac-40`, `mac-41`). As a control, 5 `-j0`
   recompiles on arm64 agreed with each other (not with `-j1`'s output), so
   `-j0`'s variance, seen once in the first `verify`, is rare.
2. **`build.sh sandboxd` could not build arm64 on the documented build
   host.** `sandbox-image/build.sh` (`rust_image_for`, `cmd_sandboxd`).
   With Docker's classic image store, running the arm64 variant of the
   pinned index after the amd64 one failed: "cannot overwrite digest". Fix:
   each platform runs by its manifest digest, read from the pinned index
   after checking the index's sha256. Test: `sandboxd` built both
   platforms, twice.
3. **Launcher unit tests failed under umask 002.**
   `rust/crates/sandbox-launcher/src/fake.rs:253-268` (`scratch`) and
   `src/trust/tests.rs:10-20` (`tree`). The checked walk starts at the
   scratch root and checks it too; `tempfile` creates it with `0777 &
   ~umask`, so under 002 (the default for `ops` on both VMs) it was
   group-writable and refused, and 31 tests failed. Fix: both helpers set
   the root to 0755. Test: the 31 existing tests; on macOS they failed
   under umask 002 before the fix and pass under 002 and 022 after it
   (73 passed), and on both VMs (umask 0002) they pass.
4. **`real_backend` against `max_containers`.**
   `rust/crates/sandbox-container/tests/real_backend.rs:10-14, 29-50` and
   `tests/support/real.rs:68-111` (`Created::release`, `destroy` now drains
   the list, `stop_and_destroy`). Test: `real_backend` on all four runtimes
   with `max_containers` 8.

`cargo fmt` and clippy (`-D warnings`, `--all-targets --all-features`) on
macOS for `agentkit-sandbox-container` and `agentkit-sandbox-launcher`: ok;
`cargo test -p agentkit-sandbox-container` on macOS: ok. `cargo +nightly
public-api` for both crates equals the committed baselines, so no baseline
was refreshed.

Image and notice changes beyond the fixes (asked for by this work item):
`PLATFORMS=linux/amd64,linux/arm64`, `SANDBOXD_SHA256_ARM64` (manifest,
`build.sh` build argument, Dockerfile check), and
`sandbox-image/notice/agentkit-sandboxd-crates.txt` (the 33 crates `cargo
tree -p agentkit-sandboxd -e normal,no-proc-macro` lists for both musl
targets, with licences, plus the Rust standard library and musl libc),
read by `generate-notice.py` and bind-mounted by the Dockerfile. That
`rustc` links the self-contained musl `libc.a` was read from the link line
of a test program in the pinned image (`-nostartfiles`, crt objects and
`-L …/self-contained` first), not from the `agentkit-sandboxd` link.

### Deviations

- **Build host.** Every Linux build (supervisor, image, gate, test
  binaries) ran on the Mac; amd64 under Rosetta. The VMs only ran tests.
- **Step order.** `sandboxd`, `build`, `verify`, then (after the VM runs)
  `load`, `selftest`, `notice`: `selftest` refuses an image whose id is
  not in `lock/image-digests.txt`, which held v0's digests until `verify`
  wrote v1's.
- **Downloads.** The pinned Rust image's amd64 variant was pulled to the
  Mac as well as its arm64 variant (`build.sh sandboxd` needs both); the
  clippy and rustfmt components were added to a local copy of the amd64
  variant. No package was installed on a VM; VM 205 has no rsync, so files
  went over `tar` and `ssh`.
- **Gate build settings.** `CARGO_INCREMENTAL=0` and `line-tables-only`
  debug info (disk and copy size), not the workspace defaults.
- **Manual checks** ran on both runtimes of both VMs (the brief asked for
  at least one per VM), from a scratch test outside the repository.
- **The `.sandbox` swap loses the swapping operation's own output**: its
  log lived in the directory it removed. The next exec is unaffected.
- **Not rerun:** the in-process conformance with internet access, and the
  test run as root (the earlier `agentkit-sys` failure there is a test
  assumption about the checkout owner).
- **Repository gates not run:** the full battery (`node scripts/gates.mjs`),
  workspace-wide clippy and `check:arch`; clippy, fmt and tests ran for the
  crates named above only.

### Open items

- arm64 on a real launcher and runtime: only `sandbox-selftest` and a
  start of the supervisor ran on arm64.
- The token read back from `Config.Env` by an `ensure` of a running
  generation: no `ensure` in these runs named a workspace and generation
  twice (the four launcher logs, 46 or 47 each), so that read through
  Podman's compat API is still unverified (ADR 0039).
- The NOTICE names the licences of the crates in `agentkit-sandboxd`, not
  their full texts, and nothing checks the committed crate list against
  `cargo tree` when `Cargo.lock` changes.
- ADR 0039 ("Permissions") still calls the 0710 parent under Docker
  inferred, not probed; this run observed it on both VMs.
- `Created::release` takes the workspace off its list before stopping it,
  so after a failed release between cases the final cleanup skips that
  workspace (the test fails either way; left as it ran).

### Cleanup

- VMs: in root's Docker and `tester`'s Podman only
  `localhost/agentkit-sandbox:v1` (the final image) is left; v0, the
  provisional `e2c39b76…` image and `rust:1.97.0-alpine` were removed. No
  containers, volumes or build cache, no launcher unit, no state or socket
  directories, no `/srv/aks` or `/srv/agentkit`, no `/etc/hosts` line, no
  `/tmp/aks-*` file (`ubuntu-90-cleanup.log`, `debian-90-cleanup.log`).
  The accounts `ops`, `tester` and `aksrv` stay (with `tester`'s empty
  `~/.local/state` from the first run). Both VMs are shut down.
- Mac: `build.sh clean` removed the BuildKit builder and its 4.5 GB state
  volume, the cargo cache volume and the supervisor target directories;
  the gate's target volume, the clippy image, both Rust image variants and
  the loaded v1 images were removed by name. Docker's totals are back to
  their values before the run. Kept: `sandbox-image/out/image.oci.tar`,
  `out/agentkit-sandboxd-{amd64,arm64}` and `out/build-{1,2}.digests`
  (not committed); the v0 images from the earlier run were left.
