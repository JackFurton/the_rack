#!/usr/bin/env bash
#
# Turns the linked ELF into a flat binary and prints its path.
#
# QEMU boots the two formats differently, and the difference is the whole
# reason this script exists. Handed an ELF, QEMU assumes a bare metal program:
# it sets the program counter to the entry point and nothing else, so x0 is
# zero and no device tree is built at all. Handed a flat image, QEMU follows
# the Linux arm64 boot protocol: it loads the image at RAM base + 0x80000,
# builds a device tree describing the machine it just assembled, and passes
# that blob's address in x0.
#
# The 0x80000 is not a coincidence. `linker.ld` already loads at 0x4008_0000,
# which is exactly where the protocol says to put the image, because that is
# the address the kernel was written to expect long before it could read a
# device tree.
#
# It is also how the machine at tier 8 will boot. Real firmware loads an image,
# not an ELF.

set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${TARGET:-aarch64-unknown-none-softfloat}"
PROFILE="${PROFILE:-debug}"
ELF="${1:-target/$TARGET/$PROFILE/kernel}"
BIN="$ELF.bin"

# `rust-objcopy` ships with the llvm-tools component and is the one that is
# certain to understand an aarch64 object on any host. The others are fallbacks
# for a machine that has binutils but not that component.
find_objcopy() {
    local candidate
    for candidate in "$(rustc --print sysroot)"/lib/rustlib/*/bin/rust-objcopy; do
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return
        fi
    done

    for candidate in llvm-objcopy aarch64-linux-gnu-objcopy gobjcopy objcopy; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return
        fi
    done

    echo "no objcopy found; try: rustup component add llvm-tools" >&2
    exit 1
}

"$(find_objcopy)" -O binary "$ELF" "$BIN"
echo "$BIN"
