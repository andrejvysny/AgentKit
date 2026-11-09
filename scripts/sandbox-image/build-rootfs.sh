#!/bin/sh
# build-rootfs.sh — reproducible trusted rootfs (registry/rootfs.ext4 +
# .sha256 sidecar). Unprivileged: builds via `mke2fs -d` (no loop mounts).
#
# Layout contract (guest agent + worker expect):
#   /init                      -> execs the guest agent (supervisor)
#   /agentkit/agent            -> static guest agent binary (admin-provided)
#   /agentkit/AGENT.sha256     -> agent content digest actually installed
#   /agentkit/IMAGE_META.json  -> provenance marker
#   /workspace                 -> private data disk mount point (per VM)
#
# Usage:
#   ./build-rootfs.sh --agent-bin <path> [--alpine-tar cache/mini.tgz]
#                     [--size 256M] [--distro alpine|none]
# `--distro none` produces a minimal busybox rootfs (needs BUSYBOX=).
set -eu
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

AGENT=""
ALPINE_TAR=""
SIZE="256M"
DISTRO="alpine"
ALPINE_URL="${ALPINE_URL:-https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz}"
while [ $# -gt 0 ]; do
    case "$1" in
        --agent-bin) AGENT="$2"; shift 2 ;;
        --alpine-tar) ALPINE_TAR="$2"; shift 2 ;;
        --size) SIZE="$2"; shift 2 ;;
        --distro) DISTRO="$2"; shift 2 ;;
        *) echo "unknown flag $1" >&2; exit 2 ;;
    esac
done
[ -n "$AGENT" ] && [ -f "$AGENT" ] || { echo "rootfs: --agent-bin required" >&2; exit 2; }
case "$SIZE" in *[!0-9MmKkGg]*) echo "bad --size" >&2; exit 2 ;; esac

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/root"

case "$DISTRO" in
    alpine)
        if [ -z "$ALPINE_TAR" ]; then
            mkdir -p cache
            ALPINE_TAR="cache/alpine-minirootfs.tar.gz"
            [ -f "$ALPINE_TAR" ] || curl -fL --retry 3 -o "$ALPINE_TAR" "$ALPINE_URL"
        fi
        tar -C "$STAGE/root" -xzf "$ALPINE_TAR" --exclude='dev/*'
        ;;
    none)
        BUSYBOX="${BUSYBOX:-$(command -v busybox || true)}"
        [ -n "$BUSYBOX" ] || { echo "rootfs: BUSYBOX= required for --distro none" >&2; exit 1; }
        mkdir -p "$STAGE/root/bin" "$STAGE/root/sbin" "$STAGE/root/proc" \
                 "$STAGE/root/sys" "$STAGE/root/dev" "$STAGE/root/tmp"
        cp "$BUSYBOX" "$STAGE/root/bin/busybox"
        for app in sh ls cat echo mkdir rm mount umount ln cp mv grep sed awk; do
            ln -sf busybox "$STAGE/root/bin/$app"
        done
        ;;
    *) echo "unknown --distro" >&2; exit 2 ;;
esac

# Inject the agent + init contract.
mkdir -p "$STAGE/root/agentkit" "$STAGE/root/workspace"
install -m 0755 "$AGENT" "$STAGE/root/agentkit/agent"
sha256sum "$AGENT" | awk '{print $1}' > "$STAGE/root/agentkit/AGENT.sha256"
cat > "$STAGE/root/init" <<'INIT'
#!/bin/sh
# Guest PID 1: the AgentKit sandbox guest agent (supervisor).
export PATH=/agentkit:/bin:/sbin:/usr/bin:/usr/sbin
exec /agentkit/agent
INIT
chmod 0755 "$STAGE/root/init"
cat > "$STAGE/root/agentkit/IMAGE_META.json" <<META
{"schema":1,"distro":"$DISTRO","agent_sha256":"$(awk '{print $1}' "$STAGE/root/agentkit/AGENT.sha256")","built_by":"scripts/sandbox-image/build-rootfs.sh"}
META
chmod 0644 "$STAGE/root/agentkit/IMAGE_META.json"

mkdir -p registry
OUT="registry/rootfs.ext4"
rm -f "$OUT"
# mke2fs positional arg = 4096-byte block count
BLOCKS="$(case "$SIZE" in *G) expr "${SIZE%G}" \* 262144 ;; *M) expr "${SIZE%M}" \* 256 ;; *K) expr "${SIZE%K}" / 4 ;; *) expr "$SIZE" / 4096 ;; esac)"
# Unprivileged build: file uid/gid reflect the builder (1000-range). The
# rootfs mounts read-only and the guest agent is PID 1 (uid 0 in-guest), so
# ownership is cosmetic; recorded in IMAGE_META.json for audit.
mke2fs -q -F -t ext4 -d "$STAGE/root" -b 4096 "$OUT" "$BLOCKS"
sha256sum "$OUT" | awk '{print $1}' > "$OUT.sha256"
echo "built $OUT ($(wc -c < "$OUT") bytes)"
