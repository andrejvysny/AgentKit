#!/bin/sh
# build-initramfs.sh — build the sandbox initramfs (embedded into the kernel
# by build-kernel.sh via CONFIG_INITRAMFS_SOURCE).
#
# The initramfs mounts the read-only trusted root (/dev/vda), formats the
# private data disk (/dev/vdb) ONLY when the kernel cmdline carries
# `agentkit.format_data=1` AND the disk carries no filesystem (fresh
# generations — a persistent disk is never silently reformatted), mounts it
# at /workspace, then switch_roots into the trusted root and execs /init
# (the guest agent supervisor). Guest-side formatting exists because macOS
# Virtualization.framework hosts have no mkfs.ext4; one image works on both
# backends.
#
# Requires: cpio, gzip; busybox (static preferred); optionally mkfs.ext4 and
# blkid (env overrides BUSYBOX=, MKFS_EXT4=, BLKID=).
set -eu

OUT="${1:-$(pwd)/initramfs.cpio.gz}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cd "$STAGE"
mkdir -p bin sbin lib dev proc sys newroot

copy_bin() {
    # copy_bin <name> [env-override]
    src=""
    if [ -n "${2:-}" ] && [ -x "${2:-}" ]; then src="$2"; fi
    [ -n "$src" ] || src="$(command -v "$1" 2>/dev/null || true)"
    [ -n "$src" ] || return 1
    cp "$src" "bin/$1"
    if command -v ldd >/dev/null 2>&1; then
        ldd "$src" 2>/dev/null | awk '/=> \// {print $3}' | while read -r lib; do
            [ -f "$lib" ] || continue
            base="$(basename "$lib")"
            case "$base" in linux-vdso*|ld-*) continue ;; esac
            [ -e "lib/$base" ] || { mkdir -p lib; cp -L "$lib" "lib/$base" 2>/dev/null || true; }
        done
    fi
    return 0
}

if ! copy_bin busybox "${BUSYBOX:-}" && ! copy_bin busybox ""; then
    echo "initramfs: busybox required (set BUSYBOX=/path/to/busybox)" >&2
    exit 1
fi
for app in sh mount switch_root mknod mkdir cat echo umount; do
    ln -sf busybox "bin/$app"
done
copy_bin mkfs.ext4 "${MKFS_EXT4:-}" || \
    echo "initramfs: WARNING mkfs.ext4 absent — guest-side formatting disabled"
copy_bin blkid "${BLKID:-}" || true

cat > init <<'INIT'
#!/bin/sh
set -e
mount -t proc  proc /proc 2>/dev/null || true
mount -t sysfs sys  /sys  2>/dev/null || true
mount -t devtmpfs dev /dev 2>/dev/null || true

FORMAT=0
for arg in $(cat /proc/cmdline); do
    [ "$arg" = "agentkit.format_data=1" ] && FORMAT=1
done

if [ -b /dev/vdb ]; then
    HAVEFS=0
    if [ -x /bin/blkid ]; then
        /bin/blkid -o value -s TYPE /dev/vdb >/dev/null 2>&1 && HAVEFS=1
    fi
    if [ "$FORMAT" = 1 ] && [ "$HAVEFS" = 0 ]; then
        /bin/mkfs.ext4 -q -F /dev/vdb || echo "init: mkfs failed" >&2
    fi
fi

/bin/mount -t ext4 -o ro /dev/vda /newroot
/bin/mount -t ext4 -o rw /dev/vdb /newroot/workspace 2>/dev/null || \
    echo "init: data disk mount failed (fail closed downstream)" >&2

exec /bin/switch_root /newroot /init
INIT
chmod +x init

[ -e dev/console ] || mknod -m 600 dev/console c 5 1 2>/dev/null || true
[ -e dev/null ] || mknod -m 666 dev/null c 1 3 2>/dev/null || true

find . | cpio -o -H newc 2>/dev/null | gzip -9n > "$OUT"
echo "built $OUT"
