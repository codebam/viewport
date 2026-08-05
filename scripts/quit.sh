#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Ask a running compositor to quit, over its control socket.
#
# `viewport msg -t quit` does the same thing and finds the socket the same way.
# This stays because it needs nothing built: a checkout with no compiled binary,
# or a session started from a package that is being replaced, still has bash and
# python3.
#
# The safe way to stop one from another TTY. It targets a socket rather than a
# process name, which matters on a machine where the C build is also called
# "viewport" and is running the desktop you are sitting in front of.
#
#   ./scripts/quit.sh                 # the newest viewport socket
#   ./scripts/quit.sh /path/to.sock   # a specific one
set -euo pipefail

runtime="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

if [ $# -ge 1 ]; then
    socket="$1"
else
    # Newest first: the one just started is the one being escaped from.
    socket=$(ls -t "$runtime"/viewport-*.sock 2>/dev/null | head -1 || true)
fi

if [ -z "${socket:-}" ] || [ ! -S "$socket" ]; then
    echo "no viewport control socket found in $runtime" >&2
    echo "sockets are named viewport-<wayland-display>.sock" >&2
    exit 1
fi

echo "asking $socket to quit" >&2

# python3 rather than socat, which is not always installed. The protocol is
# newline-delimited JSON, so this is the whole client.
python3 - "$socket" <<'PY'
import socket
import sys

path = sys.argv[1]
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.settimeout(5)
    connection.connect(path)
    connection.sendall(b'{"type":"quit"}\n')
PY

echo "sent" >&2
