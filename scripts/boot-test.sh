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
    "lock self test: passed"
    "frame self test: passed"
    # Proves the MMU is not merely on but actually restricting things: each of
    # these is an access that was refused and recovered from.
    "paging self test: passed"
    "write to .text   : permission fault"
    "write to .rodata : permission fault"
    "read low half    : translation fault"
    # Checks the values each task carried across a switch, not just that both
    # tasks ran. Right order with wrong values means the switch lost state.
    "task self test: passed"
    "locals intact"
    # Preemptive switches specifically. The spinners contain no yield, so each
    # one is the timer taking the CPU from something not finished with it.
    "preemption self test: passed"
    "tier 2: preemptive scheduling online"
    "tier 0 complete"
    "heartbeat started"
    # Exact tick counts, not just "some output happened". Reaching 200 ticks
    # means 200 timer interrupts were raised, forwarded by the GIC, claimed,
    # dispatched, and acknowledged. A missing EOI or a dropped re-arm shows up
    # here as the counter stalling.
    "uptime 1s (100 ticks)"
    "uptime 2s (200 ticks)"
)

# The run is not finished until the machine has proved it keeps running on its
# own, so wait for the second heartbeat rather than the last banner line.
READY_MARKER="uptime 2s"

# Strings that fail the run no matter what else showed up. Reaching the banner
# is not enough if the kernel fell over immediately afterwards.
FORBIDDEN=(
    "kernel panic"
)

# Build first, rather than testing whatever binary happens to be in target/.
#
# Not paranoia. Editing a source file and re-running this script would
# otherwise silently re-test the previous build, which reads as "my change did
# nothing" or, worse, "a bug I already fixed is still there". CI happens to
# build in a separate step so it never hit this; a person iterating locally
# hits it immediately.
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    cargo build --quiet
fi

if [ ! -f "$KERNEL" ]; then
    echo "boot-test: $KERNEL not found, and the build did not produce it" >&2
    exit 1
fi

WORK="$(mktemp -d)"
LOG="$WORK/serial.log"
STATUS_FILE="$WORK/qemu.status"
PID_FILE="$WORK/qemu.pid"

# Kill QEMU on every exit path, including a failed assertion, a set -e abort,
# or Ctrl-C. A guest that is never told to stop runs forever: it gets
# reparented to launchd and sits there burning a core, and nothing later in
# this script will ever find it again.
cleanup() {
    if [ -f "$PID_FILE" ]; then
        kill "$(cat "$PID_FILE")" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# QEMU runs inside a subshell so its exit code can be recorded, which is how we
# tell "still running" from "exited on its own". kill -0 cannot tell us that,
# because an unreaped child still answers it.
#
# The subshell writes QEMU's own PID to a file rather than letting the caller
# assume $! is QEMU. It is not: $! here is the subshell, and killing the
# subshell leaves the guest running.
(
    qemu-system-aarch64 \
        -machine virt \
        -cpu cortex-a72 \
        -m 256M \
        -display none \
        -serial "file:$LOG" \
        -semihosting-config enable=on,target=native \
        -kernel "$KERNEL" 2>"$WORK/qemu.stderr" &
    qemu_pid=$!
    echo "$qemu_pid" >"$PID_FILE"

    qemu_status=0
    wait "$qemu_pid" || qemu_status=$?
    echo "$qemu_status" >"$STATUS_FILE"
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
    elif grep -qF "$READY_MARKER" "$LOG" 2>/dev/null; then
        booted=1
    fi

    sleep 0.1
done

if [ -f "$STATUS_FILE" ]; then
    qemu_status="$(cat "$STATUS_FILE")"
else
    qemu_status="running"
    # Kill the guest, not the subshell wrapping it. Killing the subshell
    # orphans QEMU instead of stopping it.
    kill "$(cat "$PID_FILE")" 2>/dev/null || true
fi
wait "$RUNNER_PID" 2>/dev/null || true

# Nothing should be left behind by the time the report prints.
if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "boot-test: warning, QEMU $(cat "$PID_FILE") is still alive" >&2
fi

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
