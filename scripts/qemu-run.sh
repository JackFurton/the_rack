#!/usr/bin/env bash
#
# `cargo run`'s runner. Cargo hands us the linked ELF; QEMU wants the flat
# image, for the reasons in `image.sh`.
#
# Ctrl-A then X quits.

set -euo pipefail

BIN="$("$(dirname "$0")/image.sh" "$1")"

# A disk, so the machine has a virtio block device on it. Kept next to the
# kernel rather than in a temporary directory, so its contents survive between
# runs once something is writing to it.
DISK="$(dirname "$1")/disk.img"
[ -f "$DISK" ] || dd if=/dev/zero of="$DISK" bs=512 count=64 status=none

exec qemu-system-aarch64 \
    -machine virt \
    -cpu cortex-a72 \
    -m "${MEMORY:-256M}" \
    -nographic \
    -semihosting-config enable=on,target=native \
    -global virtio-mmio.force-legacy=false \
    -drive "if=none,file=$DISK,format=raw,id=hd0" \
    -device virtio-blk-device,drive=hd0 \
    -kernel "$BIN"
