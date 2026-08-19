#!/usr/bin/env bash
#
# `cargo run`'s runner. Cargo hands us the linked ELF; QEMU wants the flat
# image, for the reasons in `image.sh`.
#
# Ctrl-A then X quits.

set -euo pipefail

BIN="$("$(dirname "$0")/image.sh" "$1")"

exec qemu-system-aarch64 \
    -machine virt \
    -cpu cortex-a72 \
    -m "${MEMORY:-256M}" \
    -nographic \
    -semihosting-config enable=on,target=native \
    -kernel "$BIN"
