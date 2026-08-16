#!/usr/bin/env bash
#
# Boots the kernel under QEMU and fails if it does not reach the banner.
#
# This is the mechanical enforcement of the project's first rule: every commit
# on main boots. Run it locally with `scripts/boot-test.sh`; CI runs the same
# script on every push.

set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${TARGET:-aarch64-unknown-none-softfloat}"
PROFILE="${PROFILE:-debug}"
KERNEL="target/$TARGET/$PROFILE/kernel"
TIMEOUT="${TIMEOUT:-10}"

# Every string that must appear on the serial console for the boot to count.
EXPECTED=(
    "the_rack"
    "aarch64 / qemu-virt"
    "exception level : EL1"
    "tier 0 complete"
)

if [ ! -f "$KERNEL" ]; then
    echo "boot-test: $KERNEL not found, run cargo build first" >&2
    exit 1
fi

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

qemu-system-aarch64 \
    -machine virt \
    -cpu cortex-a72 \
    -m 256M \
    -display none \
    -serial "file:$LOG" \
    -kernel "$KERNEL" &
QEMU_PID=$!

# Poll rather than sleeping the full timeout, so a healthy boot finishes fast.
for _ in $(seq $((TIMEOUT * 10))); do
    if grep -q "tier 0 complete" "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.1
done

kill "$QEMU_PID" 2>/dev/null || true
wait "$QEMU_PID" 2>/dev/null || true

echo "--- serial console ---"
cat "$LOG"
echo "----------------------"

STATUS=0
for line in "${EXPECTED[@]}"; do
    if ! grep -qF "$line" "$LOG"; then
        echo "boot-test: FAIL, console never showed: $line" >&2
        STATUS=1
    fi
done

if [ "$STATUS" -eq 0 ]; then
    echo "boot-test: PASS"
fi

exit "$STATUS"
