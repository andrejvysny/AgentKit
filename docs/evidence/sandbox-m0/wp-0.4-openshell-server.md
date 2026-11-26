# WP-0.4 OpenShell server spike (Track A, Linux server)

This records the WP-0.4 spike for Milestone 0 of the Agent Sandbox spec, run on 2026-11-25 on the homelab Proxmox node. It covers D-30 (server hosts), the "Packaging → Server" paragraph and criteria A-2, A-3, A-9, A-10, A-13, A-14 and A-16.

It is a throwaway spike. No AgentKit or OneMind code was changed. Compose files, scripts and raw logs live outside git in `/Users/andrejvysny/workspace/onemind-workspace/spikes/openshell-server/`, called `$SPIKE` below. Logs are in `$SPIKE/logs/`.

## Verdict

**Recommended verdict for Track A on the server: GO, with conditions.**

- **What works.** OpenShell 0.1.10 runs as a compose service on:
  - Ubuntu 24.04 VMs, with Docker and with rootless Podman;
  - Debian 13 VMs, with Docker CE and with rootless Podman.
- **What passes.** On those combinations, A-2, A-9, A-10 and A-14 pass. The CPU, memory and pids parts of A-16 pass. On Podman, A-2 has the restart exception below.
- **Preferred runtime: Docker 28+.** It supports in-place image updates and survives a gateway restart. Rootless Podman has the smaller socket blast radius but both exceptions below.
- **Gateway settings required:**
  - `config_delivery_mode = "push"`: polling misses the 5 s revoke target;
  - `persist_workspace = true` and `enable_sandbox_updates = true`;
  - mTLS;
  - telemetry off;
  - an image with `WORKDIR /sandbox`, since 0.1.10 persists only `/sandbox`.
- **What does not pass as written:**
  - **A-3** passes in place on Docker only. Podman has no image update. A copy migration (download `/sandbox`, create from the new image, upload) preserved the files on Podman, which meets A-3's "documented migration" clause, but OneMind would have to build it.
  - **A Podman gateway restart puts freshly created sandboxes into `Error`** ("Podman sandbox is already running generation …"). Sandboxes that have been stopped and started once recover, and `stop`+`start` repairs an `Error` one with files intact. Docker does not have this problem.
  - **A-16 disk** has no per-sandbox quota. Containment comes from the operator's filesystem layout.
  - **A-9 DNS** is answered locally with synthetic addresses rather than failing. With a positive control, no query left the host.
  - **"TLS + token"** maps to mTLS client certificates. OpenShell has no static token mode.
- **What is unsupported.** Every runtime in a Proxmox LXC guest on this node is unsupported, for three independent reasons (see A-13).

## Supported-host list (A-13 / D-30)

| Host | Runtime | Result |
|---|---|---|
| Ubuntu 24.04 VM (Proxmox KVM) | Docker 29.1.3 (Ubuntu `docker.io`) | **Supported** |
| Ubuntu 24.04 VM | rootless Podman 4.9.3 (Ubuntu `podman`; OCI runtime not recorded) | **Supported with exceptions, below the upstream minimum (Podman 5.x).** No in-place A-3, and the gateway-restart defect (both shown on Debian; the Ubuntu A-2 sandbox had been stopped and started before its restart). Upstream does not qualify 4.9. |
| Debian 13 VM (Proxmox KVM) | Docker CE 29.10.1 (download.docker.com) | **Supported** |
| Debian 13 VM | Debian `docker.io` 26.1.5 | **Unsupported.** Below the upstream minimum (Docker 28.0). Every sandbox create fails. |
| Debian 13 VM | rootless Podman 5.4.2 (Debian `podman`, OCI runtime runc 1.5.3) | **Supported with exceptions.** A-3 by copy migration only; a gateway restart sends freshly created sandboxes to `Error` until `stop`+`start`. |
| Proxmox LXC (unprivileged, `nesting=1`) on this node (kernel 6.8.12-15-pve, Landlock not enabled) | any | **Unsupported.** See A-13. |

"Proxmox VM" is covered by the two VM rows: both are Proxmox KVM guests with their own kernel. A bare-metal Ubuntu or Debian host was not tested.

Host conditions for a supported install:

- **Docker:** version 28.0 or newer. On Debian 13 that means Docker's own apt repository.
- **Rootless Podman:**
  - `dbus-user-session` installed before the user's first login, or a reboot after installing it;
  - lingering enabled (`loginctl enable-linger`);
  - the user units `podman.socket` and `podman-restart.service` enabled;
  - the compose services use `restart: always` (see A-2).
- **Recommended:** the container data root (`/var/lib/docker`, or the rootless graphroot) on its own filesystem (see A-16).

## Socket exposure (A-14)

| Component | Runtime socket | Network | Runs as |
|---|---|---|---|
| `gateway` container | **Docker:** `/var/run/docker.sock` (the root daemon). **Podman:** `$XDG_RUNTIME_DIR/podman/podman.sock` (the user's socket). It is the only mount of either. | compose network `backend`, plus a host-loopback publish `127.0.0.1:8080` | Docker: uid 0. Podman: uid 1000 via `keep-id`, the same user who owns the socket. |
| onemind-server stand-in | none (`ls /var/run/docker.sock` fails) | compose network `backend` only | 1000:1000, read-only rootfs, `cap_drop: ALL`, `no-new-privileges` |
| sandbox supervisor (one per sandbox) | none | **host network** (upstream default `supervisor_network_mode = host`) | Docker: 65534:65534. Podman: 1000. |
| sandbox workload (one per sandbox) | none | `network=none` | 1000:1000. Docker: `CapDrop ALL`, `no-new-privileges`. |

Evidence: `ubuntu-docker-a14.log`, `ubuntu-podman-a16-a14.log`, `debian-dockerce-all.log` and `debian-podman-all.log` (the `docker`/`podman inspect` mount listing for every container, plus `ss -ltnp`).

**Listeners.**
- On Debian, for both Docker and Podman (`debian-{docker,podman}-recheck.log`), an unfiltered `ss -ltnup` was diffed three ways: compose down, gateway up, and gateway up with one sandbox running.
  - The only new listener is `127.0.0.1:8080`, from `docker-proxy` or `rootlessport`.
  - A running sandbox adds none, even though its supervisor is on the host network.
- On Ubuntu only the filtered list was taken; it shows the same 8080 entry.
- From the LAN, `nc -z <guest>:8080` failed on both VMs.
- Nothing is published on a non-loopback address.
- The host-loopback publish is needed because the host-networked supervisors dial the gateway at `https://127.0.0.1:8080`, which is `grpc_endpoint`.

**Authentication.**
- The gateway runs with mTLS, using the PKI from `generate-certs --server-san gateway --server-san 127.0.0.1`.
- The stand-in connected over `https://gateway:8080` on the compose network using the client bundle and got `Authenticated (mTLS transport)`.
- Without a bundle, the CLI refuses to connect. With `--gateway-insecure` and no client certificate, the gateway refuses with `missing authorization header`.
- Supervisors authenticate with gateway-minted per-sandbox bearer JWTs, not user certificates.

**Secrets on the host.** The same-path data directory bind-mounted into the gateway (`/var/lib/openshell`, or `$OS_DATA` for Podman) holds the CA key, the server and client keys, the sandbox JWT signing key and the gateway's SQLite file. It needs root-only, or user-only, permissions and backup.

**What onemind-server needs:**
- the gateway CA, plus a client certificate and key. These are user credentials, so they are a secret to mount;
- the gateway's compose DNS name as a server SAN;
- the Rust SDK pointed at `https://gateway:8080`.

**"TLS + token":**
- The gateway's user authentication modes are mTLS, OIDC, edge JWT, and unauthenticated plaintext.
- There is no static shared token. "Token" in the spec has to mean the mTLS client certificate, or an OIDC client-credentials token from an identity provider.
- **Recommendation:** mTLS.

**Implications for the spec:**
- **Docker:** the gateway container is root-equivalent on the host. It is uid 0 with the root Docker socket. This is the accepted cost of D-31.
- **Rootless Podman:** the gateway is equivalent to the Podman user's account, which is smaller.
- **onemind-server** keeps its hardening. It needs no socket, no extra capabilities and no host network.
- **Host-networked supervisors.** On the host network, the supervisor (not the workload) can reach anything the host can. Sandbox egress therefore originates from the host IP, and any policy that allows a private address probably also opens host-local services. That last point is inferred from the topology, not tested. The workload itself stays `network=none` and is mediated.

## Setup

**Proxmox.**
- Node `pve`, PVE 8.4.14, kernel `6.8.12-15-pve`.
- Guests 204 to 206 were created for this spike. Each has 2 vCPU, 4 GiB, a 32 GiB root disk and a 10 GiB data disk on `local-ssd` (thin, discard), and uses DHCP on `vmbr0`.
- Only one guest ran at a time because the node had about 10 GB of RAM free.
- No node or host setting was changed. Cloud images were downloaded to `/root/wp04-images`, checksum-verified, imported with `qm importdisk`, and the directory was deleted afterwards. Scripts: `$SPIKE/create-vm.sh`, `$SPIKE/create-lxc.sh`.

**Guests**

| ID | Name | OS | Kernel | Landlock | IP during the spike |
|---|---|---|---|---|---|
| 204 | wp04-ubuntu2404 | Ubuntu 24.04.5 cloud image | 6.8.0-147 (Docker runs). Unattended-upgrades installed 6.8.0-151, which was active from the first reboot, so the Podman runs used it. | ABI 4, `landlock` in the LSM list | 10.0.0.120 |
| 205 | wp04-debian13 | Debian 13 genericcloud | 6.12.118+deb13-cloud | ABI 6 | 10.0.0.121 |
| 206 | wp04-lxc-debian13 | Debian 13 standard template, unprivileged, `features: nesting=1` | node's 6.8.12-15-pve | **unavailable (EOPNOTSUPP)** | 10.0.0.123 |

On every guest, seccomp offers `user_notif`, cgroup v2 is in use, and `cpu memory pids` are delegated to the user slice. Logs: `*-hostfacts.log`, `lxc-all.log`.

**OpenShell v0.1.10**

| Component | Source | Digest |
|---|---|---|
| CLI | GitHub release `openshell-x86_64-unknown-linux-musl.tar.gz` | checksum matches `openshell-checksums-sha256.txt` |
| gateway | `quay.io/ai-openshell/gateway:0.1.10` | `sha256:9ee94507…` |
| supervisor | `quay.io/ai-openshell/supervisor:0.1.10` | `sha256:3bfb2f5a…` |
| sandbox runtime | `quay.io/ai-openshell/sandbox:0.1.10` | `sha256:32464032…` |

Upstream's compose file uses `:latest`; this spike pins every image.

**Compose.**
- Two variants, `$SPIKE/compose/docker/` and `$SPIKE/compose/podman/`, each with `docker-compose.yml` and `gateway.toml`.
- Both are derived from upstream `deploy/docker/docker-compose.yml` and the Podman section of `docs/how-it-works/gateways/container-deployment.mdx`.
- Changes from upstream:
  - images pinned to 0.1.10;
  - mTLS on (upstream ships plaintext with unauthenticated users);
  - `OPENSHELL_TELEMETRY_ENABLED=false` and an empty `OPENSHELL_TELEMETRY_ENDPOINT`;
  - `config_delivery_mode = "push"`;
  - `persist_workspace = true` and `enable_sandbox_updates = true` (Docker);
  - an `onemind-server` stand-in container.
- The Podman phase drives the user Podman socket with the `docker compose` CLI (`DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podman/podman.sock`). The Docker daemon is stopped during that phase.

**Telemetry.**
- The gateway env, the supervisor env and the Docker workload env carry `OPENSHELL_TELEMETRY_ENABLED=false` (`ubuntu-docker-telemetry.log`). The gateway propagates it to sandboxes.
- No network monitor was run, so A-7 is not claimed here.

**Image.**
- WP-0.2 v0 `agentkit-sandbox:v0-amd64` was saved from the Mac and loaded on each guest. Podman reports id `f04ebdf56e85…`, which matches `sandbox-image/lock/image-digests.txt`.
- Docker 29 uses the containerd store and shows the manifest digest instead (`522fe740…`). The layer diff_ids are identical to the Mac's.
- **Finding:**
  - The OpenShell persistent workspace on both Docker and Podman requires the image `WORKDIR` to be `/sandbox`. Docker: `persistent Docker workspaces currently require /sandbox`. Podman creates its managed workspace volume only for `/sandbox`.
  - v0 has `WORKDIR /workspace`. The spike therefore used `agentkit-sandbox:v0-sbx` (v0 plus `mkdir /sandbox`, `WORKDIR /sandbox`, `USER 1000:1000`) and `v1-sbx` (v0-sbx plus a marker file) for A-3. Dockerfiles are in `$SPIKE/images/sbx/`.

**Scripts.**
- `$SPIKE/guest/run-criteria.sh` runs the criteria the same way on every host and runtime. Supporting files: `probe.py`, `a9-policy.yaml`, and `update_image.py` (the A-3 gRPC client, with stubs generated from the v0.1.10 protos).
- `$SPIKE/guest/a14.sh` collects the A-14 evidence.
- `$SPIKE/phase.sh` sets a guest up for one runtime.

## Results per criterion and host

Legend: **P** pass, **F** fail, **P\*** pass with a condition stated below, **U** unsupported (did not run), **—** not applicable.

| Criterion | Ubuntu 24.04 Docker | Ubuntu 24.04 Podman 4.9 | Debian 13 Docker CE | Debian 13 Podman 5.4 | Debian 13 docker.io 26 | LXC (any) |
|---|---|---|---|---|---|---|
| A-2 persistence | P | P\* | P | P\* | U | U |
| A-2 fresh sandbox survives gateway restart | P (not separately tested) | not tested | P | **F** (until stop+start) | U | U |
| A-3 image update | P | F in place (no update RPC) | P | F in place; P\* by copy migration | U | U |
| A-9 network | P\* | P\* | P\* | P\* | U | U |
| A-10 100 MB put/get | P | P | P | P | U | U |
| A-13 runs | P | P\* (below minimum) | P | P | F | F |
| A-14 socket holder | P | P | P | P | — | — |
| A-16 cpu/mem/pids | P | P | P | P | U | U |
| A-16 full disk | P\* (layout) | P\* (layout) | P\* (layout) | P\* (layout) | U | U |

Logs:

| Host and runtime | Logs |
|---|---|
| Ubuntu, Docker | `ubuntu-docker-{a2,a2a3,a9,a9-poll,a10a16,a16,a14,reboot}.log` |
| Ubuntu, Podman | `ubuntu-podman-{a2-a10,a2a3,a16-a14,reboot}.log` |
| Debian, Docker CE | `debian-dockerce-all.log`, `debian-docker-gwrestart-observe.log` |
| Debian, Podman | `debian-podman-all.log`, `debian-podman-a2a3.log`, `debian-podman-gwrestart-observe.log`, `debian-podman-before-dbus-reboot.log` |
| Debian, docker.io 26 | `debian-dockerio26-all.log` |
| LXC | `lxc-all.log` |

### A-2 Create, exec, stop, start; persistence across gateway restart

**The sequence.** Each run did the following:

1. Create from `v0-sbx` with `persist_workspace = true` (Docker); Podman always uses a managed volume for `/sandbox`.
2. Write 1 MiB of random data to `/sandbox/a2.bin` and record its SHA-256.
3. Stop and start the sandbox, then check the hash.
4. Restart the gateway (`docker compose restart gateway`) and check again.
5. Restart the onemind-server stand-in and check again.
6. Stop and start once more and check again.

**Results.**
- `sha256sum -c` printed `OK` at every step on Ubuntu Docker, Ubuntu Podman and Debian Docker.
- On Debian Podman, exec right after the gateway restart failed in both scripted runs (`debian-podman-all.log`, `debian-podman-a2a3.log`): `boundary connection lost` or `not ready`. My `wait_ready` had read a stale Ready.
- The post-restart `OK` on Debian Podman is in `debian-podman-gwrestart-observe.log` and the A-3 re-run at the end of `debian-podman-a2a3.log`. Both are for an a2 that had already been stopped and started.

**Fresh sandboxes across a gateway restart** (`debian-podman-gwrestart-trials.log`, `debian-docker-recheck.log`):
- **Podman:**
  - A sandbox restarted by the gateway without ever having been stopped and started went to `Error`. The gateway logs `Failed to start sandbox during gateway startup … Podman sandbox is already running generation <id>`.
  - Reproduced 4 of 4 times. An A/B run with `fa` (fresh) and `fb` (stopped and started once) gave `fa` Error and `fb` Ready.
  - `sandbox stop` + `start` recovered `fa`, and `/sandbox` was intact.
- **Docker:** the same A/B gave both Ready within 1.8–3.6 s.
- This looks like an upstream Podman-driver defect in 0.1.10. It is not filed.

**Timings.**

| | Ubuntu Docker | Ubuntu Podman | Debian Docker | Debian Podman |
|---|---|---|---|---|
| create → Ready | 1.9 s | 1.3–2.0 s | 3.4 s | 1.4–2.7 s |
| start → Ready | 1.5 s | 0.8 s | 2.7 s | 0.9 s |
| first exec | 0.05–0.3 s on every combination | | | |

**Host reboot** was tested on Ubuntu only:
- **Docker:** the gateway restarted (`unless-stopped`) and brought all sandboxes back to Ready. Data was intact (`ubuntu-docker-reboot.log`).
- **Rootless Podman:**
  - Nothing came back until `podman-restart.service` was enabled for the user.
  - That unit only restarts containers whose policy is `restart: always`; `unless-stopped` stays down.
  - With both in place, the gateway came back and a2 and a9 were Ready with data intact. a16 was still `Provisioning` when checked (`ubuntu-podman-reboot.log`).

**What persists.**
- Only `/sandbox` persists.
- Under the default policy, `/home/agent` and `/workspace` are not writable (`Permission denied`). The default filesystem policy grants read-write only to the workdir, `/tmp` and `/dev/null`.
- `/tmp` survives stop and start because the same container is restarted.
- **Gap for the adapter:**
  - The spec's `/workspace` and `/home/agent` have to move under `/sandbox`, for example with `HOME=/sandbox/home`; or
  - OpenShell has to support a custom workspace root, which 0.1.10 rejects.

**The gateway restart restarts every sandbox workload.**
- The workload container's `StartedAt` changed across a gateway restart on both Debian Docker and Debian Podman: `debian-docker-gwrestart-observe.log`, `debian-podman-gwrestart-observe.log`.
- The gateway log shows `Started sandbox during gateway startup`; `start_persisted_sandboxes` relaunches with fresh authentication.
- Running processes die. `/tmp` and `/sandbox` survive.
- The phase shows Ready → Provisioning → Ready within about 5 s. `exec` during that window fails with `not ready` or `boundary connection lost`.
- This matches D-14: after a gateway restart, the adapter must record in-flight commands as interrupted.

P\* for Podman: the restart and reboot conditions above, plus the fresh-sandbox defect.

### A-3 Image update keeps files

**Docker: pass on both hosts.**
- Sequence: stop a2, then `SandboxUpdates/UpdateSandbox` with `image=…:v1-sbx` and the current `expected_resource_version`, then start.
- The client was `update_image.py` over mTLS. There is no CLI command for this.
- Results: resource version 60 → 62, `/etc/agentkit-image-update-marker` reads `v1`, and `/sandbox/a2.bin: OK`.
- Requirements:
  - `persist_workspace = true` and `enable_sandbox_updates = true`;
  - workdir `/sandbox`;
  - an explicit numeric `USER`;
  - both images cached locally;
  - no image-declared volumes.

**Podman: fail on both hosts.**
- With `enable_sandbox_updates = true`, the call returns `UNIMPLEMENTED: compute driver does not support openshell.compute.update-sandbox-configuration`.
- Without that setting it returns `FAILED_PRECONDITION: sandbox updates are disabled`.
- **Copy migration** (Debian Podman, `debian-podman-a3-copy-migration.log`):
  1. `sandbox download mig /sandbox/proj <dir>/proj`, a tree with a 5 MiB file and a subdirectory;
  2. create `mig2` from `v1-sbx`;
  3. `sandbox upload mig2 <dir>/proj /sandbox/`.
- Result: marker `v1`, and both files `OK` against the hashes from the old sandbox.
- Caveat: `download` of a directory into an existing destination writes the directory's contents, not the directory itself. The destination has to name the directory.

### A-9 Network policy

The test sandbox was created with `a9-policy.yaml`, an explicit policy with no network rules. Probes ran inside it with `probe.py`.

| Probe (no grants) | Result on all four combinations |
|---|---|
| TCP to 1.1.1.1:443 by address | `EACCES` (denied) |
| TCP to LAN gateway 10.0.0.1:80 | `EACCES` |
| UDP DNS query to 1.1.1.1:53 | `EDESTADDRREQ` (no route; fails) |
| host loopback 127.0.0.1:22 (sshd) and 127.0.0.1:8080 (gateway) | `ECONNREFUSED`. The workload is in its own `network=none` namespace, so it reaches its own loopback, not the host's. |
| host LAN IP:22, docker bridge 172.17.0.1:22 | `EACCES` |
| `getaddrinfo("example.com")` | **resolves** to 198.18.0.2 |
| `getaddrinfo(<random>.does-not-exist.invalid)` | **resolves** to 198.18.0.3 |

**DNS.** It does not fail as A-9 words it. OpenShell's in-sandbox policy DNS answers every name with a synthetic address from 198.18.0.0/15, then denies the connection (`DENIED example.com [reason:policy_dns_ineligible]`). Nonexistent names also "resolve".

A host `tcpdump` on port 53 captured **0 packets** during the probes on all four combinations. Two caveats:
- On Ubuntu, tcpdump's stderr was discarded and it was not confirmed to be installed, so the Ubuntu zeros are weak.
- On Debian, for both runtimes, the re-check ran a positive control first: host-side lookups showed 4–6 packets to 8.8.8.8, then the in-sandbox probes added 0 (`debian-{docker,podman}-recheck.log`).

So nothing is queried upstream, and DNS cannot be used to exfiltrate. I suggest rewording A-9 to "DNS gives no upstream resolution".

**PyPI.**
- Before any grant, pip fails.
- A grant for `pypi.org` alone is **not enough**: pip then fails on `files.pythonhosted.org` with `EACCES`.
- A grant for both, binary `/usr/bin/python3.13` (the real path from `readlink -f`), `read-only` `rest` `enforce`, gives `Successfully installed six-1.17.0`.
- All other probes stay closed afterwards.

**Draft, approve, revoke**

| | Ubuntu Docker | Ubuntu Podman | Debian Docker | Debian Podman |
|---|---|---|---|---|
| denied curl → draft visible | 6.2 s | 6.2 s | 5.6 s | 5.6 s |
| approve → first HTTP 200, no restart (push) | 0.26 s | 0.28 s | 0.50 s | 0.30 s |
| revoke → first failure (push) | 0.10 s | 0.13 s | 0.10 s | 0.15 s |

Details:
- The blocked `curl https://example.com` became a draft, `allow_example_com_443`, for `/usr/bin/curl`, with prover "no new findings".
- Earlier blocked probes also produced drafts (for example `allow_10_0_0_120_22`), flagged "internal/private address".
- The draft was approved with `openshell rule approve --chunk-id`. That is the CLI over the same `ApproveDraftChunk` RPC; the Rust SDK was not used here.
- The revoke was `policy update --remove-rule`, measured with an in-sandbox curl loop every 0.2 s.

**Push versus poll.** With the default `config_delivery_mode = "poll"` (Ubuntu Docker, `ubuntu-docker-a9-poll.log`):
- approve → first 200 took **10.2 s**;
- revoke → first failure took **5.4 s**, which **misses the ≤ 5 s target**.

The supervisor polls every 10 s (`OPENSHELL_POLICY_POLL_INTERVAL_SECS`). `config_delivery_mode = "push"` is therefore required, and is a condition of the P\*.

Other conditions of the P\*: the DNS wording, and two grants for PyPI.

### A-10 Put and get 100 MB

`openshell sandbox upload` and `download` (the supervisor's streaming upload, not exec stdin, which is capped at 4 MiB) moved 100 MiB of random data. The SHA-256 matched locally, in the sandbox and after download on all four combinations.

| | Ubuntu Docker | Ubuntu Podman | Debian Docker | Debian Podman |
|---|---|---|---|---|
| upload | 2.6 s | 2.8 s | 2.7 s | 2.8 s |
| download | 2.9 s | 3.3 s | 3.0 s | 2.9 s |

Download is confined to the sandbox working directory (upstream docs). `/sandbox` is that directory here.

### A-13 Hosts

**Ubuntu 24.04 and Debian 13 VMs.**
- Kernels 6.8 and 6.12. Landlock ABI 4 and 6. seccomp `user_notif` present.
- The sandbox runs admission probes (Landlock, seccomp, task-memory, `SO_BINDTODEVICE`) and fails closed if any fails. Every sandbox here reached Ready, so they passed; I infer that from Ready, since the probe output was not captured. On Ubuntu with Docker, the workload's `/proc/self/status` shows `Seccomp: 2`, 6 filters, `NoNewPrivs: 1`, `CapEff: 0`.

**Debian 13 with `docker.io` 26.1.5.** Every create fails:

```
BoundaryConfigWriteFailed: stage Docker sandbox bundle: Docker responded with status code 500: getent unable to find entry "1000:1000" in passwd database
```

This is consistent with upstream's minimum of Docker 28.0. Listed unsupported. Docker CE 29.10.1 from Docker's repository works.

**Ubuntu 24.04 with Podman 4.9.3.** Upstream says "Podman 5.x", but everything tested passed. Listed as supported below the upstream minimum: OneMind should either require Podman 5 or accept a qualification it owns.

**Debian 13 with Podman 5.4.2**, first attempt (`debian-podman-before-dbus-reboot.log`):
- The scripted sandbox creates failed because the `v0-sbx` image was missing.
- Building that image with `podman build` failed with `runc create failed … unable to apply cgroup` and the warning "no systemd user session" (seen interactively, not in the log). The gateway container itself had started.
- `/run/user/1000/bus` did not exist: `dbus-user-session` had been installed after the user's systemd instance started.
- After a reboot the bus existed, the build worked and every sandbox ran.
- Recorded as a host prerequisite. Its effect on OpenShell sandbox creation itself was not isolated.

**Proxmox LXC 206** (unprivileged, `nesting=1`, the node's kernel 6.8.12-15-pve): **unsupported on this node, for three independent reasons.** Evidence: `lxc-all.log`. OpenShell's own Landlock refusal was never reached, because no runtime could start an OpenShell container. Reason 1 rests on the kernel probe plus upstream's documented requirement.

1. **No Landlock.**
   - The node boots with LSMs `lockdown,capability,yama,apparmor`. The kernel has `CONFIG_SECURITY_LANDLOCK=y`, but `CONFIG_LSM` omits it and there is no `lsm=` on the cmdline.
   - Inside the LXC, `landlock_create_ruleset(…VERSION)` returns `EOPNOTSUPP`.
   - OpenShell requires ABI ≥ 3 and fails closed.
   - Enabling Landlock means changing the node's kernel command line. That affects every guest and is a node setting, so it was not done.
2. **Docker CE 29.10.1 with runc 1.5.3.** This is specific to that runc on this node's kernel and LXC/AppArmor stack; an older runc was not tried. Every container except `--network host`, including the gateway image itself, fails with `open sysctl net.ipv4.ip_unprivileged_port_start file: reopen fd 8: permission denied`.
3. **Rootless Podman 5.4.2:**
   - The default subuid range (`100000:65536`) does not fit inside the LXC's own idmap (`0 100000 65536`), so `newuidmap` fails.
   - To probe further, the subuid range was narrowed inside the guest. Rootless networking then needs `/dev/net/tun`, which `nesting=1` does not provide.
   - `--network none` hits the same runc sysctl error (`net.ipv4.ping_group_range`); Podman here also used runc 1.5.3, and crun was not tried. OpenShell workloads always use `network=none`.

No further LXC config (keyctl, AppArmor overrides, device passthrough, idmap changes) was tried, as the brief required. D-30's LXC row was a "Should", so this does not block A-13's Must.

### A-14

Pass on all four combinations. See "Socket exposure" above.

### A-16 Limits

Create options: `--cpu 1 --memory 512Mi`. The values were read from the host cgroup of the workload container, because the sandbox's Landlock policy hides `/sys/fs/cgroup` from the workload.

| Limit | Applied (every combination) | Behaviour |
|---|---|---|
| CPU | `cpu.max 100000 100000` | Two busy loops for 5 s used 5.02–5.11 CPU-s, with throttling counted |
| Memory | `memory.max 536870912`, `memory.swap.max 536870912` | Allocating 1 GiB: exit 137, `oom_kill 1`, and the sandbox still serves exec |
| Processes | `pids.max 2048` (OpenShell default, `sandbox_pids_limit`) | `fork` refused after 2000 children with `EAGAIN`; `pids.events max 2` |
| Disk | **none.** `StorageOpt` is null, and the Docker and Podman drivers have no quota code. | See below |

**Full disk.**
- Container storage sat on the dedicated 10 GiB `/data` disk.
- Filling `/sandbox` failed the command with `ENOSPC`. Ubuntu Docker wrote real data (9448 MiB). The other three combinations used `posix_fallocate` (8–9 GiB), to avoid growing the node's thin pool.
- In every case the host root filesystem stayed writable and the gateway stayed `Connected` (its SQLite is on the root disk).
- But the fill is **not contained per sandbox**: other sandboxes on the same data disk got `I/O error` or `write FAILED`, and on Podman `/tmp` in the filling sandbox failed too.
- With the default layout, where the data root is on `/`, the same fill would reach the host's root filesystem.
- So "a full disk fails the command, not the host" holds only because of the operator layout, and only between the host and the sandboxes, not between sandboxes. A per-sandbox quota would need an XFS project quota, which neither driver sets, or a `tmpfs size_bytes` workspace, which is not persistent.

## Deviations and notes

- The A-9 draft approval used the CLI, not the Rust SDK. Both call the same `ApproveDraftChunk` RPC.
- A-3 used a Python gRPC client built from the 0.1.10 protos, because there is no CLI for `UpdateSandbox`.
- The `v0-sbx` and `v1-sbx` images are spike-only adaptations of WP-0.2 v0 (workdir `/sandbox`).
- Debian 13 Docker used Docker's official repository, because Debian's own `docker.io` is below the upstream minimum. That combination is listed separately.
- In the LXC, the in-guest subuid change was made only to locate the next blocker. It is not part of any supported row.
- `config_delivery_mode = "push"` and `persist_workspace = true` are non-default gateway settings. They are needed for A-9 latency and A-2/A-3.

## Guests to delete

All were left **stopped**, with no containers inside. They are not deleted: the IDs are reported for the lead to approve deletion.

| ID | Type | Name | Disks on `local-ssd` |
|---|---|---|---|
| 204 | VM | wp04-ubuntu2404 | `vm-204-disk-0` 32G, `vm-204-disk-1` 10G, `vm-204-cloudinit` |
| 205 | VM | wp04-debian13 | `vm-205-disk-0` 32G, `vm-205-disk-1` 10G, `vm-205-cloudinit` |
| 206 | LXC | wp04-lxc-debian13 | `vm-206-disk-0` 32G (rootfs), `vm-206-disk-1` 10G (mp0) |

The `local-ssd` thin pool went from 72.7 % before the spike to 80.2 % after, with all three guests stopped. Deleting them returns that space.
