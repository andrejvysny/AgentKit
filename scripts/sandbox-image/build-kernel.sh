#!/bin/sh
# build-kernel.sh — reproducible sandbox guest kernel with the initramfs
# embedded. Output: registry/kernel.bin (+ .sha256 sidecar).
#
# Direct-boot (no bootloader): Firecracker boots the ELF; macOS
# VZLinuxBootLoader takes the same uncompressed binary.
#
# Usage:
#   ./build-kernel.sh [--version 6.12.8] [--source <tar|dir>] [--jobs N]
# Env: KERNEL_URL_TEMPLATE (default kernel.org), CC override.
# The initramfs is rebuilt first so the kernel embeds the current one.
set -eu
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

VERSION="6.12.8"
SOURCE=""
JOBS="$(nproc)"
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --source) SOURCE="$2"; shift 2 ;;
        --jobs) JOBS="$2"; shift 2 ;;
        *) echo "unknown flag $1" >&2; exit 2 ;;
    esac
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p registry

./build-initramfs.sh "$WORK/initramfs.cpio.gz"

if [ -n "$SOURCE" ]; then
    SRC="$(cd "$SOURCE" && pwd)"
elif [ -f "cache/linux-$VERSION.tar.xz" ]; then
    tar -C "$WORK" -xf "cache/linux-$VERSION.tar.xz"
    SRC="$WORK/linux-$VERSION"
else
    mkdir -p cache
    BASE="${KERNEL_URL_TEMPLATE:-https://cdn.kernel.org/pub/linux/kernel/v6.x}"
    curl -fL --retry 3 -o "$WORK/linux.tar.xz" "$BASE/linux-$VERSION.tar.xz"
    tar -C "$WORK" -xf "$WORK/linux.tar.xz"
    SRC="$WORK/linux-$VERSION"
fi

cd "$SRC"
defconfig
FRAG="$WORK/sandbox.config"
cat > "$FRAG" <<'FRAG'
CONFIG_VIRTIO=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS_COMMON=y
CONFIG_VSOCKETS=y
CONFIG_BLOCK=y
CONFIG_EXT4_FS=y
CONFIG_TMPFS=y
CONFIG_PROC_FS=y
CONFIG_SYSFS=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_SERIAL_8250_PCI=n
CONFIG_INET=n
CONFIG_NET=n
CONFIG_BINFMT_SCRIPT=y
CONFIG_SECCOMP=y
CONFIG_SECCOMP_FILTER=y
CONFIG_NAMESPACES=y
CONFIG_PID_NS=y
CONFIG_NET_NS=y
CONFIG_USER_NS=y
CONFIG_FUTEX=y
CONFIG_EPOLL=y
CONFIG_MAGIC_SYSRQ=n
CONFIG_INITRAMFS_SOURCE="INITRAMFS_PLACEHOLDER"
CONFIG_KALLSYMS=y
FRAG
sed -i "s|INITRAMFS_PLACEHOLDER|$WORK/initramfs.cpio.gz|" "$FRAG"
scripts/kconfig/merge_config.sh -m .config "$FRAG"
make -j"$JOBS" vmlinux
mkdir -p "$ROOT/registry"
cp vmlinux "$ROOT/registry/kernel.bin"

cd "$ROOT"
sha256sum registry/kernel.bin | awk '{print $1}' > registry/kernel.bin.sha256
echo "built registry/kernel.bin ($(wc -c < registry/kernel.bin) bytes)"
