#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# What a shell sees when the compositor quits.
#
# The order is the whole point. An out-of-process shell is told the session is
# over by its control socket closing, and it stops its engine when that
# happens — so the compositor has to close that socket *before* it takes the
# Wayland display away. Quitting used to be a plain drop in declaration order,
# which took the display first: servoshell went down winit's broken-pipe path,
# ran its exit handlers on a half-torn-down engine, and faulted inside them.
# The compositor then reaped a shell it had already killed and called that a
# clean exit.
#
# Neither half of that is visible to a unit test — it is two processes and the
# order between them — so it is tested here with a stub in place of the engine.
# `VIEWPORT_SHELL_BIN` is the seam the compositor already has for this.
#
# Two cases, because the quit has two paths:
#
#   * A shell that stops when its socket closes must find the display still
#     up when it does.
#   * A shell that ignores the socket must be signalled rather than waited on
#     for ever.
#
#   tests/shell-stop.test.sh target/debug/viewport
set -u

viewport=${1:-target/debug/viewport}
if [ ! -x "$viewport" ]; then
	echo "missing $viewport — build first" >&2
	exit 2
fi
viewport=$(realpath "$viewport")

if ! command -v python3 >/dev/null; then
	echo "SKIP: no python3 to stand in for the shell"
	exit 77
fi

workdir=$(mktemp -d)
viewport_pid=

# By PID, and only the one this test started: pattern matching on "viewport"
# has killed a live session more than once and this runs on the same machine
# as one.
cleanup() {
	[ -n "$viewport_pid" ] && kill "$viewport_pid" 2>/dev/null
	[ -n "$viewport_pid" ] && wait "$viewport_pid" 2>/dev/null
	rm -rf "$workdir"
}
trap cleanup EXIT

failures=0
check() {
	local what=$1 expected=$2 got=$3
	if [ "$expected" = "$got" ]; then
		echo "ok: $what"
	else
		echo "FAIL: $what — wanted $expected, got $got" >&2
		failures=$((failures + 1))
	fi
}

# The stub shell.
#
# It does what every out-of-process backend does — hold the control socket and
# watch for it to close — and then records the one thing the compositor is on
# trial for: whether the Wayland display was still there at that moment. A
# connect is the honest test of that, because it is what the engine would have
# been doing when the socket went.
cat >"$workdir/stub.py" <<'PY'
import os
import signal
import socket
import sys
import time

out = os.environ["SHELL_STUB_OUT"]
linger = os.environ.get("SHELL_STUB_LINGER") == "1"


def record(line):
    with open(out, "a") as f:
        f.write(line + "\n")
        f.flush()


def on_term(_signum, _frame):
    record("sigterm")
    sys.exit(0)


signal.signal(signal.SIGTERM, on_term)

control = socket.socket(socket.AF_UNIX)
control.connect(os.environ["VIEWPORT_IPC_SOCKET"])
record("connected")

while True:
    if not control.recv(4096):
        break

record("socket-closed")

if linger:
    # The shell that will not take the hint. The compositor has to end this
    # itself rather than wait on it.
    while True:
        time.sleep(0.05)

display = os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"])
probe = socket.socket(socket.AF_UNIX)
try:
    probe.connect(display)
    record("display-up")
except OSError as e:
    record("display-down: %s" % e)
PY

# One run of the compositor with the stub as its shell, quit by its own
# deadline. Prints nothing; the stub's file is the result.
#
# `--exit-after` rather than a signal, because this compositor installs no
# signal handling: a `SIGINT` ends it where it stands and never reaches the
# shutdown under test. The deadline is the same path the Exit chord takes.
run_with_stub() {
	local out=$1 linger=$2
	local log="$workdir/viewport-$linger.log"
	: >"$out"
	SHELL_STUB_OUT="$out" \
		SHELL_STUB_LINGER="$linger" \
		VIEWPORT_SHELL_BIN="$workdir/stub.py" \
		"$viewport" --headless --exit-after 3 >"$log" 2>&1 &
	viewport_pid=$!

	for _ in $(seq 1 200); do
		kill -0 "$viewport_pid" 2>/dev/null || break
		sleep 0.1
	done
	wait "$viewport_pid" 2>/dev/null
	viewport_pid=
}

# The stub is spawned directly, so it has to be executable and carry its own
# interpreter line.
sed -i '1i #!/usr/bin/env python3' "$workdir/stub.py"
chmod +x "$workdir/stub.py"

out="$workdir/stub.out"
run_with_stub "$out" 0

check "the shell was started and connected" yes \
	"$(grep -q connected "$out" && echo yes || echo no)"
check "its control socket closed at quit" yes \
	"$(grep -q socket-closed "$out" && echo yes || echo no)"
check "and the display was still up when it did" yes \
	"$(grep -q display-up "$out" && echo yes || echo no)"

out="$workdir/stub-linger.out"
run_with_stub "$out" 1

check "a shell that ignores the socket is signalled" yes \
	"$(grep -q sigterm "$out" && echo yes || echo no)"

exit $((failures > 0))
