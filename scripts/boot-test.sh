#!/usr/bin/env bash
#
# Boots the kernel under QEMU and fails if it does not reach the banner.
#
# This is the mechanical enforcement of the project's one rule: main boots. Run
# it locally with `scripts/boot-test.sh`; CI runs the same script on every push.
#
# A healthy kernel never exits, so the success path is "the banner showed up,
# now shut the emulator down". A panicking kernel brings QEMU down itself via
# semihosting, which we notice immediately rather than waiting out the timeout.

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
    # The trap self test only prints this after taking a real BRK exception,
    # building a frame, decoding it, and returning through eret. If the save or
    # restore paths are broken it hangs or faults instead of reaching here.
    "class  : BRK instruction"
    "trap self test: resumed"
    "tier 0 complete"
)

# Strings that fail the run no matter what else showed up. Reaching the banner
# is not enough if the kernel fell over immediately afterwards.
FORBIDDEN=(
    "kernel panic"
)

if [ ! -f "$KERNEL" ]; then
    echo "boot-test: $KERNEL not found, run cargo build first" >&2
    exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
LOG="$WORK/serial.log"
STATUS_FILE="$WORK/qemu.status"

# Wrapping QEMU in a subshell that records its exit code is the reliable way to
# tell "still running" from "exited on its own"; kill -0 cannot, because an
# unreaped child still answers.
(
    qemu-system-aarch64 \
        -machine virt \
        -cpu cortex-a72 \
        -m 256M \
        -display none \
        -serial "file:$LOG" \
        -semihosting-config enable=on,target=native \
        -kernel "$KERNEL" 2>"$WORK/qemu.stderr"
    echo $? >"$STATUS_FILE"
) &
RUNNER_PID=$!

booted=0
settle=0

# Keep watching for half a second after the banner lands. Otherwise a kernel
# that prints the banner and then immediately panics looks identical to a
# healthy one, and the test passes on a machine that is already dead.
SETTLE_TICKS=5

for _ in $(seq $((TIMEOUT * 10))); do
    # QEMU exiting by itself means the kernel called semihosting exit, which
    # today only happens on a panic.
    if [ -f "$STATUS_FILE" ]; then
        break
    fi

    if [ "$booted" -eq 1 ]; then
        settle=$((settle + 1))
        if [ "$settle" -ge "$SETTLE_TICKS" ]; then
            break
        fi
    elif grep -qF "tier 0 complete" "$LOG" 2>/dev/null; then
        booted=1
    fi

    sleep 0.1
done

if [ -f "$STATUS_FILE" ]; then
    qemu_status="$(cat "$STATUS_FILE")"
else
    qemu_status="running"
    kill "$RUNNER_PID" 2>/dev/null || true
fi
wait "$RUNNER_PID" 2>/dev/null || true

echo "--- serial console ---"
cat "$LOG" 2>/dev/null || true
echo "----------------------"

status=0

if [ "$qemu_status" != "running" ] && [ "$qemu_status" != "0" ]; then
    echo "boot-test: kernel brought QEMU down with status $qemu_status" >&2
    status=1
fi

if [ "$booted" -eq 0 ] && [ "$qemu_status" = "running" ]; then
    echo "boot-test: FAIL, kernel never reached the banner within ${TIMEOUT}s" >&2
    status=1
fi

for line in "${EXPECTED[@]}"; do
    if ! grep -qF "$line" "$LOG" 2>/dev/null; then
        echo "boot-test: FAIL, console never showed: $line" >&2
        status=1
    fi
done

for line in "${FORBIDDEN[@]}"; do
    if grep -qF "$line" "$LOG" 2>/dev/null; then
        echo "boot-test: FAIL, console showed: $line" >&2
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "boot-test: PASS"
fi

exit "$status"
