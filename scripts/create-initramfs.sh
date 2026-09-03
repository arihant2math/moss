#!/usr/bin/env bash
set -euo pipefail

# Error if cpio is not installed
if ! command -v cpio &> /dev/null; then
    echo "cpio not found"
    exit 1
fi

base="$( cd "$( dirname "${BASH_SOURCE[0]}" )"/.. && pwd )"
pushd "$base" &>/dev/null || exit 1

archive="$base/initramfs.cpio"

if [ -f "$archive" ]; then
    rm "$archive"
fi

# Error if the alpine rootfs has not been extracted yet
if [ ! -d "$base/build/rootfs" ]; then
    echo "$base/build/rootfs not found, run scripts/create-image.sh"
    exit 1
fi

# Lay out the directories in $base/build/initramfs that init expects
if [ -d "$base/build/initramfs" ]; then
    rm -rf "$base/build/initramfs"
fi
mkdir -p build/initramfs/{bin,lib,dev,proc,sys,tmp,new-root}

# Copy busybox over and symlink the stuff that init uses
cp "$base/build/rootfs/bin/busybox" "$base/build/initramfs/bin/"
for applet in sh mount mkdir pivot_root chroot; do
    ln -s busybox "$base/build/initramfs/bin/$applet"
done

# Copy the musl loader and libc that busybox is linked against
cp -a "$base/build/rootfs"/lib/ld-musl-*.so.* "$base/build/rootfs"/lib/libc.musl-*.so.* "$base/build/initramfs/lib/"

# make archive
# init mounts the real root off the virtio disk and switches over to it
cat > "$base/build/initramfs/init" <<'INIT'
#!/bin/sh

mount -t ext4 /dev/vda /new-root
mkdir -p /new-root/old-root

cd /new-root
mkdir -p dev
mount -t devfs devfs dev

pivot_root . old-root

exec chroot . /bin/sh <dev/console >dev/console 2>&1
INIT
chmod +x "$base/build/initramfs/init"

(cd "$base/build/initramfs" && find . | cpio -o -H newc) > "$archive"
