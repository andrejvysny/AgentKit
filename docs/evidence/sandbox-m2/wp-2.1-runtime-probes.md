# WP-2.1 container runtime probes (Track B, Linux server)

Probes run on 2026-11-25 on the homelab Proxmox node to check the container-runtime
behaviour that ADR 0039 (Sandbox Track B server backend) depends on. No AgentKit code
was used: plain `docker` / `podman` CLI plus raw Engine API calls over the unix socket.
Scripts and raw logs are outside git in
`/Users/andrejvysny/workspace/onemind-workspace/spikes/m2-probes/` (`$SPIKE` below;
logs in `$SPIKE/logs/<vm>-<probe>.log`, scripts in `$SPIKE/guest/`).

## Verdict

ADR 0039's template works on Docker 29 and rootless Podman 4.9 / 5.4, with six changes
to the launcher (see "Consequences"). The two that would break it as written:

- **No portable bare digest.** Docker 29 with its default containerd image store knows
  the image only by the **index** digest; Podman knows it only by the **config** digest.
  Docker's classic overlay2 store was not tested. The one reference that
  works on every runtime is `localhost/agentkit-sandbox@sha256:<index digest>`, and it
  needs the image to carry that name. The OCI tar has no ref name, so a load leaves it
  untagged on both runtimes.
- **Podman's compat API maps `PidsLimit: 0` to `pids.max = 1`.** The launcher must
  always send a positive `PidsLimit`. The template does, but a zero tier value would
  produce a container that cannot fork.

## Hosts

| | VM 204 `m2-ubuntu2404` | VM 205 `m2-debian13` |
|---|---|---|
| OS / kernel | Ubuntu 24.04.5, 6.8.0-151-generic | Debian 13, 6.12.118+deb13-cloud-amd64 |
| Proxmox | 4 vCPU (host), 8 GiB, 40 GiB `local-ssd` (thin, discard), DHCP on `vmbr0`, MAC `BC:24:11:0D:68:3B` | same; MAC `BC:24:11:A4:B7:EB` |
| IP (DHCP lease) | 10.0.0.125 | 10.0.0.127 |
| Docker | docker-ce 29.10.1 (download.docker.com), API 1.57, containerd 2.4.2, runc 1.5.3, storage `overlayfs` with **containerd snapshotter**, cgroup v2 / systemd | same versions |
| Podman (rootless, `tester` uid 1001) | 4.9.3 (Ubuntu), compat API 1.41, OCI runtime runc 1.5.3, netavark, overlay | 5.4.2 (Debian), compat API 1.41, runc 1.5.3, netavark, overlay |
| Rootless delegation | `cpu memory pids` (no `io`, `cpuset`) | same |
| Swap | none | none |

- Installed packages (both): `docker-ce docker-ce-cli containerd.io podman uidmap
  dbus-user-session python3` plus their dependencies (`$SPIKE/guest/provision.sh`; exact
  versions in `*-hostfacts.log`). Podman uses runc because `containerd.io` provides it;
  crun was not installed.
- `tester`: subuid/subgid `165536:65536`, linger on, `systemctl --user enable --now
  podman.socket` → `/run/user/1001/podman/podman.sock`. It survived a `qm shutdown` +
  `qm start` on both VMs without a login. Container uid 1000 maps to host uid 166535.
- The cloud-init user `ops` has uid 1000 and passwordless sudo. Docker runs rootful.
- `rust:1.97.0-alpine` (matches `rust/rust-toolchain.toml`) was pulled into Docker on both
  VMs: `sha256:ec9c91e77119ce498cd1e87d96d77e0f75b2cee21655a29bc2bf75a51a2b20a4`,
  rustc 1.97.0, Alpine 3.24.1.
- Access: `ssh -F $SPIKE/ssh/config m2-ubuntu | m2-ubuntu-tester | m2-debian |
  m2-debian-tester` (key `$SPIKE/ssh/id_ed25519`). If a lease changes, find the IP from
  the node: ping-sweep 10.0.0.0/24, then `ip -4 neigh | grep -i <MAC>`.

"Compat" below means the `docker` CLI pointed at Podman's socket
(`DOCKER_HOST=unix:///run/user/1001/podman/podman.sock`). That is how the launcher will
talk to Podman.

## 1. Image identity

The image was loaded from `sandbox-image/out/image.oci.tar` (sha256 `171107be…`). Its
`index.json` wraps index `9847…` and has no `org.opencontainers.image.ref.name`. Digests:
index `sha256:984736245081…`, amd64 manifest `sha256:62df84af621b…`, config
`sha256:f04ebdf56e85…`.

| | Docker 29 (containerd store) | Podman 4.9 / 5.4 |
|---|---|---|
| `load` result | `Loaded image ID: sha256:9847…` (untagged, dangling) | `Loaded image: sha256:f04e…` (untagged) |
| image `Id` | **index** digest `9847…` | **config** digest `f04e…` |
| `RepoDigests` after tag | `[agentkit-sandbox@sha256:9847…]` | `[localhost/agentkit-sandbox@sha256:62df…, …@sha256:9847…]` |
| other fields | `Descriptor` = index descriptor | `Digest` = `9847…` (4.9) / `62df…` (5.4 after tag) |
| run/create by bare `sha256:9847…` (index) | yes | **no** (`image not known`) |
| by bare `sha256:62df…` (manifest) | no | no |
| by bare `sha256:f04e…` (config) | **no** | yes |
| by `name@sha256:9847…` (index) | yes | yes |
| by `name@sha256:62df…` (manifest) | **no** (CLI then tries a pull) | yes |
| container `.Image` (read back) | `sha256:9847…` | `sha256:f04e…` |
| container `.Config.Image` | the reference as given | the reference as given |
| API `POST /containers/create` with an absent digest | 404, no pull | 404, no pull |

- **Common identifier: `localhost/agentkit-sandbox@sha256:<index digest>`.** API create
  with it returned 201 on Docker, Podman 4.9 and Podman 5.4. On Docker, the image needs
  the tag `localhost/agentkit-sandbox:<x>`. Podman's own tag already lands in
  `localhost/`. Logs: `*-probe1c.log`.
- The short name `agentkit-sandbox@…` also works on both. But Docker expands it to
  `docker.io/library/…`, and on a cache miss the Docker CLI tries a pull from Docker
  Hub. A tag made through Podman compat is created as `docker.io/library/…`.
- Only the CLI pulls on a miss; the API create does not (404 on both).

## 2. Fixed template

Command (all six cells):

```
run -d --init=false --network none --cap-drop ALL --security-opt no-new-privileges --read-only \
  --tmpfs /tmp --user 1000:1000 --pids-limit 512 --memory 2g --cpus 2 <ref> sleep 600
```

| Check | Docker | Podman CLI | Podman compat |
|---|---|---|---|
| inside: `pids.max` / `memory.max` / `cpu.max` | 512 / 2147483648 / `200000 100000` | same | same |
| inside: `memory.swap.max` | 2147483648 | 2147483648 | 2147483648 |
| host cgroup (same values) | `system.slice/docker-<id>.scope` | `user@1001.service/user.slice/libpod-<id>.scope` | same as CLI |
| Caps / NoNewPrivs / Seccomp | all 0 / 1 / 2 | same | same |
| fork until refused | 510 forks then `EAGAIN`, `pids.events max 2` | same | same |
| touch 3 GiB | killed, exit 137, `oom_kill 1` | same | same |
| 4 busy loops × 3 s | 6.04 s CPU, 30 throttles | 6.17 s, 31 | 6.14–6.18 s, 30 |
| writable mounts besides `/proc`, `/dev` | `/tmp` only (`nosuid,nodev,noexec`) | `/tmp` (`nosuid,nodev`, **exec allowed**), **`/var/tmp`** (writable by 1000), `/run` (root-owned 0755, not writable) | same as CLI |
| `/tmp` size | none set (3.9G = half of RAM), mode 1777 | same | same |

- No limit was silently ignored under rootless Podman. `cpu memory pids` are delegated
  on both VMs. `io` and `cpuset` are not, so block-I/O or cpuset limits would fail
  under rootless.
- `memory.swap.max` equals the memory limit on every runtime (Docker's default
  `MemorySwap = 2 × Memory`). These VMs have no swap, so it had no effect here. On a
  host with swap, a 2 GiB sandbox could use 2 GiB more as swap.
- While `pids.max` is reached, `docker exec` / `podman exec` fail too
  (`runc init … Resource temporarily unavailable`).
- Docker API with `PidsLimit` omitted, 0 or -1 → `pids.max` 9483, so Docker sets no
  pids limit of its own. That the 9483 comes from systemd's `TasksMax` default on the
  scope is inferred, not checked. Podman compat: omitted/`null` → 2048 (containers.conf default), **0 → 1**,
  -1 → `max`. The Docker CLI sends 0 when `--pids-limit` is absent, so through compat
  such a container cannot fork. Logs: `ubuntu-probe3-compat-debug.log`,
  `debian-probe3-compat-extra.log`.

**Explicit options (probe 2c, Debian only, all three cells).** Command: the template
plus `--memory-swap 2g`, `--tmpfs /tmp:rw,nosuid,nodev,noexec,size=64m` and
`--tmpfs /var/tmp:rw,nosuid,nodev,noexec,size=32m`. Results were identical on Docker,
Podman CLI and Podman compat:

- `memory.swap.max` = 0;
- `/tmp` 64M and `/var/tmp` 32M, both `noexec` (exec of a copied binary is denied);
- `/var/tmp` writable by uid 1000.

Podman still adds its root-owned `/run` tmpfs, which uid 1000 cannot write. Log:
`debian-probe2c.log`.

## 3. Named volumes at `/workspace` and `/home/agent`

The image has both directories empty, 1000:1000, mode 0755. Fresh volumes were mounted
with the full template.

| Variant | Docker | Podman CLI | Podman compat |
|---|---|---|---|
| `-v vol:/path` or `--mount type=volume` (default copy-up) | root 1000:1000 0755, empty, writable | same | same |
| host owner of `_data` after first mount | 1000:1000 (was 0:0) | 166535:166535 (was 1001:1001) | same as CLI |
| second mount | unchanged, file persists | same | same |
| `-v …:nocopy` | 0:0 0755, **not writable** by 1000 | **create fails** (runc: `bind mounts cannot have any filesystem-specific options`) | start fails (409 on 4.9 / 500 on 5.4) |
| `--mount …,volume-nocopy` (API `VolumeOptions.NoCopy`) | 0:0 0755, not writable | `invalid mount option` | **ignored**: still chowned to 1000 and still copies content |
| copy into a non-empty image dir (`/etc/sandbox`) | copies (`bashrc environment`); nocopy → empty | copies | copies, even with `NoCopy: true` |
| volume implicitly created by run | 1000:1000, writable | same | same |

- The default (no `NoCopy`) gives exactly what is needed on every runtime: an empty
  root, owned by 1000, writable. Nothing is copied, because the image directories are
  empty.
- The ownership comes from the copy-up of the image directory's owner and mode. It
  therefore depends on the image keeping `/workspace` and `/home/agent` owned by
  1000:1000.

## 4. Unix socket across a read-only bind mount

Setup: the server user creates `<base>/ws1/1/` and listens on `control.sock`
(`$SPIKE/guest/sockserver.py`). The directory is bind-mounted with
`--mount type=bind,src=…,dst=/run/agentkit,readonly`. Inside, uid 1000 runs a Python
`connect` + `send`. Docker: server is root, base `/run/aks-p4`. Podman: server is
`tester`, base `$XDG_RUNTIME_DIR/aks-p4`. The modes were changed on the host while the
container ran.

The result was identical on all six cells. Group "1000" means both the directory and
the socket have the container's gid: host gid 1000 on Docker; on Podman,
`podman unshare chgrp 1000` gives host gid 166535.

| dir mode | socket mode | group server | group 1000 |
|---|---|---|---|
| 0700 | any | denied | denied |
| 0710 | 0660 / 0666 | denied | **OK** |
| 0711 | 0600 | denied | denied |
| 0711 | 0660 | denied | **OK** |
| 0711 | 0666 | **OK** | **OK** |
| 0755 | same as 0711 | | |

- **Minimal modes without group changes: directory 0711, socket 0666.** A freshly bound
  socket is `srwxr-xr-x` under the default umask (Docker), which is refused. The adapter
  must `chmod` it.
- With the group variant, 0710 + 0660 also works. But the Podman group is the subgid
  166535, which only `podman unshare` (or a CAP_CHOWN holder) can set.
- Inside the container the socket appears owned by `0:0` on both runtimes. On Podman
  that is because `tester` maps to container root.
- `SO_PEERCRED` seen by the server: uid/gid **1000** on Docker, **166535** on Podman.
- Read-only mount, with the directory 0777 so that only RO can refuse: `touch`,
  `mkdir`, `rm control.sock`, `mv`, `ln -s` and `bind()` of a new socket all fail with
  `EROFS`, and the socket stays.
- A server restart (unlink + re-bind of the same path) while the container runs is
  visible at once; the reconnect works.
- A bind source under `/run/user/1001` (parent 0700) works for rootless Podman.

## 5. `--network none`

| Probe (uid 1000, template) | VM host (control) | All six container cells |
|---|---|---|
| interfaces | `lo eth0 docker0` | `lo` only |
| listen 127.0.0.1:3128 + connect | OK | **OK** |
| `[::1]` listen + connect | OK | OK |
| TCP 1.1.1.1:443, TCP 10.0.0.1:80 | connected | `ENETUNREACH` |
| UDP DNS query to 1.1.1.1:53 | reply | `ENETUNREACH` |
| `getent hosts example.com` | resolves | rc=2 (fails) |

Docker still writes a `resolv.conf` with the host's nameservers; they cannot be reached.

## 6. Volume size via the Engine API

Seed: 20 000 files of 50 KiB (1 024 000 000 bytes) in one volume, 4 KiB in another.

| | Docker | Podman compat |
|---|---|---|
| `GET /system/df?type=volume` | 200; only `Volumes` + `VolumeUsage` | 200; **`type` ignored**: images, containers and build-cache sections too |
| `Volumes[].UsageData.Size` | populated, 1024000000 | populated, 1024000000 |
| `UsageData.RefCount` | 1 while mounted by a running container | same |
| time, warm cache | 0.06 s | 0.07–0.14 s |
| time, cold (`drop_caches`) | 0.30 s (both VMs) | 0.78 s (4.9) / 0.89 s (5.4) |
| `GET /v1.41/system/df` | same sizes | same |
| `GET /volumes/<name>` | no size | no size (libpod inspect has none either) |
| alternative | — | `GET /libpod/system/df` → `Volumes[] {VolumeName, Size, …}` |

- `Size` is the apparent size (`du -sb`), not the allocated size (`du -sB1` was 4 %
  larger).
- Neither runtime can filter to one volume: every call returns every volume on the
  host, and on Podman the images too. That the cost therefore grows with the host,
  not with the workspace, is inferred; only two volumes were present.

## 7. PID 1

`run -d --init=false … bash -c 'sleep 1000 & exec sleep 1000'`, then inspect `/proc`:

| | Docker | Podman CLI | Podman compat |
|---|---|---|---|
| PID 1 | `sleep 1000` | `sleep 1000` | `sleep 1000` |
| child | pid 7, ppid 1 | same | same |
| `HostConfig.Init` | false | false | `<nil>` |
| no `--init` flag at all | PID 1 = the command | same | same |
| `stop -t 2` (PID 1 ignores SIGTERM) | SIGKILL after 2.13–2.29 s | same, plus a "resorting to SIGKILL" warning | same |

No init is injected on any runtime with default daemon and containers.conf settings.

## Consequences for ADR 0039

The launcher must do the following differently from the ADR text:

1. **Image reference.** Configure the image as the **index digest** and always reference
   it as `localhost/agentkit-sandbox@sha256:<index>`. This was verified with Docker's
   default containerd image store only.
   - **Installing** the image must also name it: tag `localhost/agentkit-sandbox:<ver>`
     after `docker load` (by index digest) or `podman load` (by config digest). An
     alternative is for the build to add `org.opencontainers.image.ref.name` to
     `index.json`; that was not tested.
   - **Verifying a container's image** must use `.Config.Image` (the reference echoed
     back) or a launcher label, not `.Image`. `.Image` is the index digest on Docker and
     the config digest on Podman.
   - **The launcher stays API-only.** API create never pulls; the CLI does.
2. **Always send explicit limits.** Send `PidsLimit > 0`; Podman compat turns 0 into 1.
   Also send `MemorySwap` equal to `Memory`: with it, `memory.swap.max` was 0 on all
   cells (probe 2c). Swap use itself was not exercised, because the VMs have none.
   Send `Init: false` explicitly, so that a daemon-wide `init: true` cannot add a PID 1
   in front of the supervisor; that daemon setting was not tested.
3. **Writable paths are not uniform.**
   - Podman's `ReadonlyRootfs` adds a writable `/var/tmp` tmpfs and a root-owned
     `/run` tmpfs. Docker adds neither.
   - By default `/tmp` is `noexec` on Docker only, and is sized at half of RAM.
   - The launcher should pass `Tmpfs` with explicit options for both `/tmp` and
     `/var/tmp`, for example `rw,nosuid,nodev,noexec,size=<tier>`. Probe 2c showed that
     every runtime, including Podman compat, honours them and that the explicit
     `/var/tmp` replaces Podman's automatic one.
   - Podman's `/run` tmpfs remains. It is root-owned and harmless, and a
     `/run/agentkit` bind mount works inside it.
4. **Never set `NoCopy`.** The default copy-up already yields an empty, 1000-owned,
   writable volume root on every runtime. With `NoCopy`:
   - on Docker, the root stays `root:root` and unwritable;
   - in the Podman CLI, it fails;
   - in Podman compat, it is ignored.

   This relies on the image keeping both directories 1000:1000 and empty. A future image
   that ships files there would have them copied into fresh volumes.
5. **Socket permissions.** The per-machine directory at 0711 works as the ADR says, but
   **the adapter must `chmod 0666` each socket after `bind`**.
   - The tighter 0710/0660 + group form needs the container gid as seen by the host:
     1000 on Docker, the subgid (166535 here) on rootless Podman. An unprivileged server
     generally cannot set that.
   - The adapter must not check `SO_PEERCRED` uid == 1000: Podman reports the subuid.
     First-connection-wins remains the protection.
6. **Usage.** `GET /system/df?type=volume` is enough on both runtimes: read
   `UsageData.Size` for both `aks-*-<ws>` volumes from one call. But it walks every
   volume on the host (0.3–0.9 s cold for one 1 GB volume). The service should not call
   it before every mutating operation. It should cache with a TTL, or use the
   supervisor's `usage` as the hot path and the launcher's `usage` as a check.

Also relevant:

- **Docker maps container uid 1000 to host uid 1000.** On these VMs that is the
  cloud-init login user (`ops`): it owns every workspace file on the host and can
  connect to every 0666 control socket. Server deployments should keep uid 1000 free of
  login users, or prefer rootless Podman.
- **A workload at `pids.max` blocks `exec` into the container.** This only matters for
  debugging, since the design never uses `exec`.
- **The supervisor as PID 1 gets no default SIGTERM action.** It must handle SIGTERM
  itself, or `stop` always waits the full grace period.

## Not done / caveats

- The VMs ran one at a time (node RAM; about 12 GiB was available).
- Ubuntu's Podman is 4.9.3, from the distribution. All seven probes behaved the same as
  on 5.4.2.
- The compat cell in the first `ubuntu-probe3.log` run is invalid: it had no
  `--pids-limit`, so `pids.max` was 1. The valid rerun is `ubuntu-probe3-compat.log`.
- `crun`, Docker's classic (non-containerd) image store, and a host with swap were not
  tested.
- Probe 2c (explicit swap and tmpfs options) ran on Debian only.
- Ubuntu's cloud-init upgraded packages on first boot (kernel 6.8.0-147 → -151 before
  any probe). apt's daily timers and unattended-upgrades were then disabled on both VMs.
