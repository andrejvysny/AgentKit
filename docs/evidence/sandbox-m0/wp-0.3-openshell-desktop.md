# WP-0.3 OpenShell desktop spike (Track A, macOS)

This records the WP-0.3 spike for Milestone 0 of the Agent Sandbox spec, run on 2026-11-25. It covers the spec sections "Track A: OpenShell facts verified at v0.1.10", "Operation mapping proposal", "Packaging" and "Exit criteria for Milestone 0" (A-1 to A-16).

It is a throwaway spike. No AgentKit or OneMind code was changed. The spike code, scripts and raw logs live outside git in `/Users/andrejvysny/workspace/onemind-workspace/spikes/openshell-desktop/`, called `$SPIKE` below. Unless stated otherwise, every log named here is in `$SPIKE/evidence/logs/`.

## Setup

**Host**
- macOS 26.6.2 (25G83) on Apple Silicon (arm64). The spec names macOS 15.
- Rust 1.97.0.
- OrbStack 29.8.2 provides the Docker API.
- No Homebrew OpenShell, no system settings changed, no sudo.

**OpenShell v0.1.10**
- The repository has moved to `github.com/ai-openshell/openshell`; `NVIDIA/OpenShell` redirects there.
- Tag `v0.1.10` is commit `152c59d3178039e3fbb4a5595f9fff7216a6be92`, published 2026-11-24, and is the newest release.
- Downloaded the `aarch64-apple-darwin` tarballs for the CLI, gateway, `openshell-driver-vm` and prover. The prover was not used.
- All four match the published `*-checksums-sha256.txt`.
- All four pass `gh attestation verify -R ai-openshell/openshell` (Sigstore/SLSA). The signer is `release-publish.yml@refs/heads/main`, so the release was built from `main`, not from the tag ref. There are no detached signature files.
- Upstream already ad hoc signs every binary and gives the driver `com.apple.security.hypervisor`.

**Bundle**
- `$SPIKE/bundle/binaries/native/{openshell-gateway,openshell-driver-vm}`, re-signed with `codesign --force -s -`. The driver uses upstream `entitlements.plist` (hypervisor).
- `$SPIKE/bundle/shims/`: see finding 1.

**Spike binary** (`$SPIKE/spike`, crate `osd-spike`)
- `openshell-sdk = { git = "https://github.com/ai-openshell/openshell", tag = "v0.1.10" }`.
- `osd-spike serve` stands in for the app:
  - generates a per-install PKI with `openshell-gateway generate-certs`;
  - picks a free loopback port;
  - writes `gateway.toml` (example in `$SPIKE/evidence/gateway.toml.example`);
  - spawns the gateway as a child with a cleared environment, `OPENSHELL_TELEMETRY_ENABLED=false`, XDG directories under app data and `PATH` set to the shims;
  - health-checks it over the SDK and stops it on SIGTERM.
- The gateway spawns the driver.
- Other subcommands drive the SDK: the curated client where an operation exists, `raw` gRPC otherwise.

**Images**
- WP-0.2 v0: `agentkit-sandbox:v0-arm64`, docker id `f61e548f…`, matching `sandbox-image/lock/image-digests.txt`.
- `agentkit-sandbox:v0b-arm64`: a local derivative of v0, built for A-3.
- `debian:bookworm-slim` (local) as the bootstrap image.

**Configurations used**

| Config | Bootstrap image | Policy delivery | Used for |
|---|---|---|---|
| C1 | v0 (same as the sandbox image) | `poll` | A-2, A-4 (first set), A-5 to A-7, A-9 grant and draft, A-10 to A-12, A-16 |
| C2 | v0 | `push` | A-9 activation and revoke, A-11, A-12, A-3 |
| C3 | `debian:bookworm-slim` | `push` | A-4 (tight), A-8 (second part), per-sandbox resources, `/workspace` persistence |

Finding 3 shows C1 and C2 are the wrong production setup: they lose ownership. C3 is the recommended one.

**State location**
- VM state: `state_dir = /tmp/osd-spike` (finding 5).
- SQLite database and PKI: `$SPIKE/appdata`.

## Findings that shape the verdict

1. **Host e2fsprogs is a hard, unbundled dependency.**
   - The driver resolves `mke2fs`/`mkfs.ext4`, `debugfs` and `e2fsck` from `PATH` or `/opt/homebrew/opt/e2fsprogs`. Without them `config preflight` refuses to start (`$SPIKE/evidence/preflight-no-e2fsprogs.txt`).
   - Nothing was installed, Homebrew was off limits, and fetching e2fsprogs source was not approved.
   - The spike therefore puts shims on the gateway `PATH` that run Debian's binaries in OrbStack with `--network none`. Each call takes about 0.15 s, and starting a sandbox makes about 30 `debugfs` calls (`shim-calls.log`).
   - Every timing below includes this overhead.
   - A shipping app must bundle e2fsprogs (GPL-2 tools) or upstream must drop the dependency.
2. **On macOS, images come from a local container engine or a registry.**
   - The driver accepts three sources:
     - the local Docker/Podman engine, found through `DOCKER_HOST` or `/var/run/docker.sock`;
     - a registry reference, over HTTPS only;
     - a rootfs tar uploaded through `BeginRootfsTarStaging`, for the sandbox image only.
   - The bootstrap image must come from the engine or a registry.
   - There is no path that loads an OCI archive from disk. The conversion paths are `docker load` and then an engine reference, a registry push, or `docker export` to a rootfs tar.
   - An unqualified local name such as `agentkit-sandbox:v0-arm64` falls back to `index.docker.io` when the engine is unreachable. The driver did reach Docker Hub on the first run, which used the wrong engine socket. In A-8 the request was blocked by the dead proxy.
3. **The OneMind image must not be its own bootstrap image.**
   - When it is, the host assembles the root disk, and in the VM `/workspace`, `/home/agent` and `/etc/shadow` end up `root:root` and `/var/tmp` loses its sticky bit.
   - With a separate bootstrap image, the OneMind image is unpacked inside a bootstrap VM with umoci. Ownership and modes are then correct, and WP-0.2's `sandbox-fs write` and `sandbox-exec` deadline work (`a3-v0b-ownership.txt`, `bootstrap-separate.txt`).
4. **There is no static-token auth mode.**
   - The gateway authenticates users with mTLS, OIDC or edge JWT.
   - The spike used per-install mTLS: a random CA plus a client key, mode 0600 in app data. This stands in for the "random token".
5. **macOS limits socket paths to `sun_path` (103 usable bytes).**
   - The gateway places `compute-driver.sock` at `<state_dir>/run/compute-driver.sock`.
   - A `state_dir` under the spike tree made that path 106 bytes and failed with "path must be shorter than SUN_LEN". A symlinked `state_dir` is refused.
   - `~/Library/Application Support/com.onemind.desktop/sandbox/vm/run/compute-driver.sock` comes to 101 bytes for this user name, which leaves almost no margin.
   - The driver also creates `/tmp/os-<uid>-<random>` socket roots and leaves stale ones behind.

## Per-criterion results

| ID | Result | Evidence |
|---|---|---|
| A-1 | **Fail**, partly **blocked** | The gateway and driver run as children of the spike binary from a bundle-shaped directory. Both are re-signed ad hoc, the hypervisor entitlement works ad hoc, and Homebrew is not used. **"No Docker" is not met:** e2fsprogs runs through a Docker shim (finding 1) and image supply needs the engine or a registry (finding 2, A-8). **Notarization and Gatekeeper: blocked, no Developer ID (D-26).** `spctl` rejects the ad hoc binaries. Ran on macOS 26.6.2, not 15. `a1-signing-tree.txt`, `$SPIKE/evidence/preflight-no-e2fsprogs.txt` |
| A-2 | **Pass** | A 1 MiB random file and a directory in `/sandbox` kept the same SHA-256 across stop/start, across an app restart (gateway on a new port; the running sandbox restarted itself in 10.4 s), and through the A-11 kill series (`a11-parent-second-app.txt`). In C3, files in `/workspace` and `/home/agent` also survived stop/start, given the policy `read_write` grants. `a2-stop-start.txt`, `a2-app-restart.txt`, `a2-workspace-home-persist.txt` |
| A-3 | **Pass via migration.** In-place update: **fail** | `SandboxUpdates/UpdateSandbox` with `enable_sandbox_updates` returns `Unimplemented … update-sandbox-configuration` on VM. Migration: create a sandbox from v0b, then copy a 106 MB tar of `/sandbox` through exec streams. 25-file SHA-256 manifest OK; a pip `--user` package still imports; copy took 6.2 s. `a3-update.txt`, `a3-migration.txt` |
| A-4 | **Pass**, with the shim | Start of a stopped sandbox to first command result: 4.9–5.4 s with a 50 ms readiness poll, three runs in C3. The SDK `wait_ready` backs off 250 ms doubling to 2 s, so the earlier 5.85–5.89 s figures are inflated. About 30 shimmed `debugfs` calls (about 4.9 s) fall inside that window, so native e2fsprogs should be much faster; this was not measured. A further `true` command: p50 3.6–4.6 ms, p90 4.0–5.0 ms, max 6.4 ms (50 runs). `a4-timings*.txt` |
| A-5 | **Pass**; memory is returned only on stop | Idle with one running sandbox, RSS: gateway 68 MB + driver 123 MB + supervisor 41 MB = 232 MB (footprint 137 MB). The VM process is 187 MB RSS. On stop, the VM processes exit and the driver drops to 90 MB. There is no balloon: after a 2 GiB allocation in the guest the VM process held 2.1 GB until stop. After the 100 MB transfers the gateway was at 166 MB RSS. `a5-memory.txt`, `a16-disk.txt` |
| A-6 | **Pass** for the single-user case | The only TCP listener is `127.0.0.1:<port>`. The driver socket is 0600 in a 0700 directory, and the driver checks the peer UID and gateway PID. With no client certificate or with TLS verification off, Health answers but List and Exec get "missing authorization header". A client certificate from a second, foreign `generate-certs` CA fails the TLS handshake. Plaintext gets 404. Health and gRPC reflection are unauthenticated by design. The "bogus bearer" probe never sent a header, so it proves nothing. **Another local user:** not tested; no second account exists and none was created. Such a user can reach the loopback port but needs the 0600 client key; the CA key and the JWT signing key also sit in app data at 0600. `a6-auth-listen.txt`, `a6-auth-foreign.txt` |
| A-7 | **Pass, within the method's limits** | Telemetry off via the environment variable; the default endpoint is `events.telemetry.data.nvidia.com`. Monitor: `nettop -L 0 -n -x -s 1` plus `lsof -i -c openshell` every second over a full lifecycle (app start, start, exec, create, stop, delete, 30 s idle, app restart). All 18 sockets were loopback; 0 external. In the A-9 window the only external sockets were `openshell-supervisor` to 151.101.x.223:443 (PyPI, granted). Limits: 1 s sampling misses short connections (the 1 s approved npm fetch was not seen); DNS goes through mDNSResponder and is not attributed to processes. Outside the monitored windows the driver reached `index.docker.io` on the registry fallback (finding 2). `a7-*`, `a9-nettop-summary.txt` |
| A-8 | **Fail** | (a) Engine unreachable plus dead proxy: starting an existing sandbox and creating a new one both failed, because the driver re-resolves the image on every start (engine first, then registry) and tried `index.docker.io`. (b) Engine reachable plus dead proxy: start and create work, with 0 registry attempts. So offline start depends on Docker or Podman being up. A sandbox left in Error by (a) needed stop then start. Code read: a digest-pinned registry reference skips the network only for the bootstrap disk. The prepared-image path (`ensure_prepared_registry_image_disk`) authenticates and fetches the manifest before checking its cache. Since finding 3 forces the OneMind image to differ from the bootstrap, a registry-sourced OneMind image cannot start offline. Other than the proxy variables, the network could not be blocked without system changes. `a8-offline.txt`, `a8-offline-engine-up.txt`, `gw-a8*.log` |
| A-9 | **Pass with push delivery**, with a DNS deviation | Push meets every timing; the default poll fails revoke. **Deviation:** DNS does not fail. Unknown names resolve to synthetic `198.18.0.x` addresses and connecting to them is denied. The code (`policy_dns::stage_observation`) says "No upstream DNS request or egress grant is made". Details below. `a9-*` |
| A-10 | **Pass** | 100 MiB random file, three rounds, SHA-256 equal every time. Put streams stdin through `ExecSandboxInteractive` into `cat >` at 29–38 MB/s. Get reads `ExecSandbox` stdout at 24–34 MB/s. The gateway caps each stdin message at 1 MiB, so the spike sends 256 KiB chunks. `a10-transfer.txt` |
| A-11 | **Fail** on the parent kill | Details below. `a11-*` |
| A-12 | **Pass** | Two sandboxes running at once. Two agents sent 10 concurrent commands each to ws1 while a third sent 10 to ws3: 30 of 30 exited 0, and ws1's log interleaves all 20 A and B lines. ws3 cannot see ws1's files. `a12.txt` |
| A-15 | **Partial**; upgrade drill **blocked** | The SDK at the tag builds standalone in 25 s: 437 crates, 6 of them git (`openshell-core`, `-crypto`, `-extension-core`, `-policy-schema`, `-sdk`, `-spiffe`). The SDK reports version `0.0.0`. `cargo deny check`, run with copies of both configs: AgentKit `rust/deny.toml` passes advisories and licenses, **fails sources** (git) and **fails bans** (`axum`, pulled by `openshell-core`'s default `tonic-prost`/`tonic-types` features). OneMind `deny.toml` passes licenses and bans, **fails advisories** (RUSTSEC-2026-0543 chrono, via `oauth2` and `openshell-core` in the SDK) and **fails sources**. The SDK also pulls `openshell-core`'s default `telemetry` feature and the AWS-LC crypto backend. Not built inside the OneMind workspace, which was out of bounds. OneMind locks `prost 0.13.4` and the SDK brings `prost`/`tonic` 0.14. Upgrade drill: blocked, v0.1.10 is the newest tag. `cargo-build-sdk.txt`, `a15-*.txt` |
| A-16 | **Pass**, with limits set driver-wide only | CPU: 2 vCPUs; four busy loops show 199 % host CPU. Memory: allocating past the 2 GiB guest triggers the guest OOM killer (exit 137); the VM survives. Processes: `RLIMIT_NPROC` is 7690 and spawning fails at 7641; the VM survives. Disk: ENOSPC at 3935 MiB on the 4 GiB sparse overlay; other sandboxes and the gateway are unaffected. The host keeps 3.8 GB allocated after the guest deletes the file (no discard) until the sandbox is deleted. Per-sandbox `template.resources` (cpu 1, memory 1Gi) still gave 2 vCPUs and 2 GiB, so limits are set only by the driver-wide `vcpus`, `mem_mib` and `overlay_disk_mib`. `/dev/zero` is not in the default Landlock list. `a16-*.txt` |

### A-9 details

**No grants** (`a9-nogrants.txt`)
- TCP to an address: EACCES.
- UDP: `EDESTADDRREQ`. The guest has only `lo` and `dummy0`, no NIC.
- Guest loopback to the gateway port: refused.
- Host LAN IP, `10.0.2.2`, `192.168.64.1`, `192.0.0.8`: EACCES.
- `host.openshell.internal`: unresolvable.
- Each denial became a draft. Private-address drafts carry security notes.

**Grant** (`a9-grant-pypi.txt`)
- Granted with `UpdateConfig` merge `add_rule`: `pypi.org` and `files.pythonhosted.org`, port 443, for `/usr/bin/python3.13`.
- `pip install --user six` then works without a restart.
- curl stays denied, because rules apply per binary.
- The supervisor terminates TLS; `SSL_CERT_FILE` and its siblings point at the sandbox CA.

**Draft** (`a9-draft.txt`)
- A denied `registry.npmjs.org:443` appears in `GetDraftPolicy` within the denial flush, about 10 s.
- `ApproveDraftChunk` (chunk id plus `review_token`) takes 16 ms. The first successful retry came 3.8 s later, without a restart.
- `review_token` changes between reads.

**Activation and revoke**

| Delivery mode | Activation | Live keep-alive connection closed after revoke |
|---|---|---|
| Default `poll` | 5.2–9.2 s | 6.8 s, **over the 5 s limit** |
| `config_delivery_mode = "push"` | 102–106 ms | 48 ms, host-side timestamps |

In both modes, new connections after a revoke get EACCES.

### A-11 details

**Gateway SIGKILL**
- VM launchers and the supervisor exit within 0.03 s (driver procguard).
- The exec stream ends with `h2 protocol error`.
- After the app restarts, state is readable, but exec is refused for 18–45 s ("owned by gateway replica … no peer endpoint") until the VM relaunches.
- Writes the command made in its last ~4 s were lost.

**Driver SIGKILL**
- The VMs and supervisor exit within 0.03 s.
- The stream ends with `Unavailable: exec relay closed before the command reported an exit status`.
- The gateway keeps running and **Health still reports OK**, but exec reports "sandbox is not ready" and create reports "Connection refused".
- Nothing respawns the driver. Only a host-initiated gateway restart recovers, after which the sandbox was usable in 7.4 s.
- A file synced before the kill survived; unsynced writes were lost.

**App parent SIGKILL**
- The gateway, driver, VM and supervisor are orphaned to launchd; macOS has no parent-death signal.
- The VM was alive at +10 s and +15 s, and the command ran to completion.
- **Starting the app again started a second gateway on the same `state_dir` and database. It launched a second VM for the same sandbox while the orphan's VM was still running.** `ps` showed both launcher sets alive. `lsof` was run only after the orphan stopped, so concurrent opens of `overlay.ext4` were not captured.
- There is no single-instance lock.
- No corruption was seen afterwards, but this is a two-writer ext4 hazard.

**Detecting the outcome**
- A broken stream is always detectable. It means the outcome is unknown; it never produces a false exit code.

**Related finding**
- After an app restart in A-8(b), 2 of 3 running-intent sandboxes failed to restart themselves with `wait for image preparation: Operation not permitted (os error 1)`.
- Stop followed by start recovered both (`autorestart-eperm-recover.txt`).

## Key numbers

| Measure | Value |
|---|---|
| Gateway spawn to healthy | 199–333 ms |
| First create, v0 as its own bootstrap, via CLI | 40.9 s |
| First create with image prepared in a bootstrap VM | 19.8–23.8 s (SDK backoff, ±2 s) |
| Cached create | 4.9–5.1 s (tight poll) |
| Start of stopped sandbox to first result | 4.9–5.4 s, mostly shim calls |
| Exec round trip | p50 3.6–4.6 ms |
| Graceful gateway stop with a VM running | 426 ms |
| `StopSandbox` | 0.43–1.2 s |
| Policy activation, poll / push | 5–9 s / about 0.1 s |
| 100 MiB put / get | 29–38 / 24–34 MB/s |
| Idle RSS, gateway + driver + supervisor | 232 MB |
| Idle RSS, VM process | 187 MB |
| Bootstrap cache, v0 / Debian slim | 680 MB / 96 MB |
| Prepared v0 image cache | 1.3 GB |
| Overlay | sparse 4 GiB |

## Gaps against the operation mapping

- **stdin streaming: confirmed.**
  - `ExecSandboxInteractive` with `tty=false` streams stdin in messages of at most 1 MiB.
  - The curated SDK `exec` is buffered, with stdin as a single blob.
  - `ExecSandboxRequest.request_id` ("durable launch admission … duplicates never relaunch") exists, which narrows the "no operation id or dedupe" gap. It was not tested.
- **Cancel kills the tree: no.**
  - Dropping an `ExecSandbox` stream left the whole `sh` tree running 60 s later (`gap-cancel.txt`).
  - With `sandbox-exec --marker`, cancelling and then running `sandbox-kill` ended the group, and the wrapper then ended the `setsid` escapee.
  - Under OpenShell, `/proc/<pid>/environ` of processes from another exec session is EACCES. `sandbox-kill`'s environment scan therefore relies on the recorded group and the live wrapper. This is a caveat for the WP-0.2 helper.
- **Persistence across image update: not supported on VM.**
  - `UpdateSandbox` is Unimplemented; use the A-3 migration.
  - Writes persist in the overlay upper layer: `/sandbox`, which is `HOME`, plus `/workspace` and `/home/agent` once the policy grants them `read_write`.
  - `/tmp` is tmpfs.
  - pip `--user` installs to `/sandbox/.local`, not `/home/agent`.
- **Draft policy API: works.**
  - Raw `GetDraftPolicy` and `ApproveDraftChunk` (needs `review_token`).
  - `proposal_approval_mode` defaults to `manual`.
  - Drafts appear on a flush interval of about 10 s.
- **Other gaps:**
  - `SuspendSandbox` is Unimplemented on VM.
  - Per-sandbox resource settings are ignored.
  - Health does not reflect a dead driver.
  - There is no single-instance lock and no parent-death handling.
  - Stale `/tmp/os-501-*` socket roots are left behind.
  - `generate-certs` leaves `ca.key` beside the certificates, which are valid until 4096.

## Recommended verdict for Track A on desktop

**Not acceptable as tested; conditionally viable.**

What works inside the VM:
- Isolation and egress policy, with push delivery.
- Persistence.
- Exec latency and file transfer.
- Resource limits.
- Concurrent sandboxes.

Three groups of Must failures remain:

1. **A Docker-free runtime (A-1, A-8) needs upstream changes or OneMind additions.**
   - Bundle e2fsprogs.
   - Get images in without an engine. Today an offline start needs either a running Docker or Podman, or an OCI-layout/archive source with a cache-first prepared-image path, which does not exist yet.
   - Digest pinning covers only the bootstrap disk.
2. **The host must own the lifecycle (A-11).**
   - A lock and pidfile on `state_dir`, killing a stale gateway at startup.
   - A watchdog or launchd job that kills the gateway when the app dies.
   - A driver liveness check beyond Health.
3. **Required settings.**
   - `config_delivery_mode = "push"`.
   - A `bootstrap_image` different from the OneMind image.
   - `read_write` policy grants for `/workspace` and `/home/agent`.
   - A short `state_dir`.

**Next step:** if OneMind accepts bundling e2fsprogs and either keeps a container engine available or funds the upstream change, Track A stays the lower-effort track. Re-run A-1, A-4 and A-8 with native e2fsprogs before the M0 decision.

## Reproduce

1. Build: `cargo build` in `$SPIKE/spike`.
2. Start: `$SPIKE/scripts/serve.sh <tag> --push --bootstrap debian:bookworm-slim`. Other `serve` flags: `--no-docker`, `--offline`, `--updates`, `--image`.
3. Drive it with `$SPIKE/spike/target/debug/osd-spike` and one of these subcommands: `create`, `exec`, `bench`, `put`, `get`, `run`, `drafts`, `approve`, `rule`, `stop`, `start`, `delete`, `update-image`, `authprobe`.

Helper scripts in `$SPIKE/scripts/`:
- `a11.sh` and `a11-time.sh`;
- `netprobe.py`, `keepalive.py`, `limits.py` and `ps.sh`, which are pushed into the sandbox;
- `nettop-filter.py`;
- `ts.py`, which timestamps output on the host.

Prerequisites:
- OrbStack running.
- `debian:bookworm-slim` and `agentkit-sandbox:v0-arm64` loaded.
- The v0b derivative rebuilt from `$SPIKE/image-v0b/Dockerfile`.

The raw `nettop` CSVs are trimmed to openshell processes.

## Cleanup

All processes started by the spike are stopped; `ps` filtered on the spike paths shows none.

**Removed**
- Every sandbox, deleted through the API.
- `agentkit-sandbox:v0b-arm64`.
- The 100 MB transfer files.
- The `/tmp/os-501-*` socket roots.
- Every generated private key: the CA, server, client and JWT keys under `appdata` and the CLI's copy.

**Not removed, because `rm -rf` was refused by the session's permission policy:**

| Path | Size | Contents |
|---|---|---|
| `/tmp/osd-spike` | 3.5 GB | VM image caches; sandboxes already deleted |
| `$SPIKE/spike/target` | 885 MB | build output |
| `$SPIKE/extract` | 182 MB | duplicate of the bundle binaries |
| `$SPIKE/upstream-src` | 61 MB | v0.1.10 source clone |

To remove them: `rm -rf /tmp/osd-spike $SPIKE/spike/target $SPIKE/extract $SPIKE/upstream-src`.

**Kept for reproduction**
- `downloads` (92 MB)
- `bundle` (161 MB)
- `scripts`
- `spike/src`
- `evidence` (3.3 MB)
- the rest of `appdata` (28 MB, no keys)
