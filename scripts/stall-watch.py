#!/usr/bin/env python3
"""Measure whether the compositor's event loop is actually responding.

The session freezes for tens of seconds while the process stays alive. The
first attempt at catching this watched the log for silence, which does not
work: an idle compositor logs nothing either, so "quiet" and "wedged" look
identical and every reading was suspect.

This asks the compositor a question instead. It sends output.query on the
control socket and waits for the output.layout that comes back. That round trip
goes through the same event loop that dispatches input and drives repaints, so
the time it takes is a direct measurement of how long the loop is unavailable —
independent of the log, of whether anyone is typing, and of what the screen is
doing.

When a probe takes longer than the threshold, every thread's backtrace is
captured while the stall is still happening. Several samples through one stall
are worth more than one: the frame they share is the answer.

    ./scripts/stall-watch.py                       # auto-detects the socket
    ./scripts/stall-watch.py --threshold 1.5

Needs to attach with ptrace, so run it as root (run0) or set
kernel.yama.ptrace_scope=0. Writes ~/viewport-stalls.txt and prints a running
summary.
"""
import argparse
import glob
import json
import os
import pwd
import shutil
import socket
import subprocess
import sys
import time


def find_socket():
    for pattern in ("/run/user/*/viewport-*.sock", "/tmp/viewport-*.sock"):
        hits = sorted(glob.glob(pattern))
        if hits:
            return hits[-1]
    return None


def find_gdb():
    """run0 hands over root's PATH, which does not include the user's nix
    profile — so the obvious lookup finds nothing and every capture comes back
    empty. Look where it actually is."""
    found = shutil.which("gdb")
    if found:
        return found
    candidates = ["/run/current-system/sw/bin/gdb"]
    candidates += glob.glob("/etc/profiles/per-user/*/bin/gdb")
    candidates += glob.glob("/home/*/.nix-profile/bin/gdb")
    candidates += sorted(glob.glob("/nix/store/*-gdb-*/bin/gdb"))
    for c in candidates:
        if os.access(c, os.X_OK):
            return c
    return None


def viewport_pid():
    try:
        out = subprocess.run(["pgrep", "-x", "viewport"],
                             capture_output=True, text=True, timeout=5)
        pids = [int(p) for p in out.stdout.split()]
        return pids[-1] if pids else None
    except Exception:
        return None


def probe(path, timeout):
    """Round-trip one request. Returns seconds, or None if it never answered."""
    started = time.monotonic()
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(timeout)
        s.connect(path)
        # Drain the state the compositor pushes at every new client, so the
        # reply being waited for is the one this asked for.
        deadline = started + timeout
        s.sendall(b'{"type":"output.query"}\n')
        buf = b""
        while time.monotonic() < deadline:
            chunk = s.recv(1 << 16)
            if not chunk:
                break
            buf += chunk
            if b'"output.layout"' in buf:
                s.close()
                return time.monotonic() - started
        s.close()
        return None
    except (socket.timeout, TimeoutError):
        return None
    except OSError:
        return None


def capture(gdb, pid, out, label):
    with open(out, "a") as f:
        f.write("=" * 64 + "\n")
        f.write("%s — %s\n" % (label, time.strftime("%T")))
        try:
            with open("/proc/%d/wchan" % pid) as w:
                f.write("wchan: %s\n" % w.read().strip())
        except OSError:
            pass
        f.write("-" * 64 + "\n")
        f.flush()
        try:
            r = subprocess.run(
                [gdb, "-p", str(pid), "-batch",
                 "-ex", "set pagination off",
                 "-ex", "thread apply all bt 30"],
                capture_output=True, text=True, timeout=40)
            noise = ("[New LWP", "[Thread debugging", "Using host lib",
                     "Reading symbols", "warning: ")
            for line in (r.stdout + r.stderr).splitlines():
                if not line.startswith(noise):
                    f.write(line + "\n")
        except subprocess.TimeoutExpired:
            f.write("gdb timed out\n")
        f.write("\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default=None)
    ap.add_argument("--threshold", type=float, default=2.0,
                    help="seconds of unresponsiveness that counts as a stall")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    path = args.socket or find_socket()
    if path is None:
        sys.exit("no viewport control socket found")

    out = args.out
    if out is None:
        # Under run0 $HOME is root's, and run0 may give the unit a private /tmp
        # where anything written is invisible from outside. Put it in the home
        # of whoever owns the socket.
        owner = pwd.getpwuid(os.stat(path).st_uid).pw_name
        out = os.path.expanduser("~%s/viewport-stalls.txt" % owner)

    gdb = find_gdb()
    if gdb is None:
        sys.exit("gdb not found (needed for backtraces)")

    if os.geteuid() != 0:
        try:
            scope = int(open("/proc/sys/kernel/yama/ptrace_scope").read())
        except OSError:
            scope = 0
        if scope != 0:
            sys.exit("need root (run0) or kernel.yama.ptrace_scope=0")

    # Prove the attach works before waiting for a stall. Both previous attempts
    # at this looked like they were running and produced nothing: once because
    # ptrace was refused, once because run0 hands over root's PATH and gdb was
    # not on it. Finding that out after the stall has come and gone wastes the
    # only thing worth catching.
    probe_pid = viewport_pid()
    if probe_pid is None:
        sys.exit("no viewport process to attach to")
    check = subprocess.run([gdb, "-p", str(probe_pid), "-batch",
                            "-ex", "info threads"],
                           capture_output=True, text=True, timeout=60)
    if "Operation not permitted" in check.stderr or check.returncode != 0:
        sys.exit("gdb cannot attach to pid %d:\n%s"
                 % (probe_pid, (check.stderr or check.stdout).strip()[:400]))
    print("gdb attach verified (%s)" % gdb, flush=True)

    print("probing %s every 0.5s; stalls >%.1fs -> %s"
          % (path, args.threshold, out), flush=True)
    open(out, "w").close()

    worst = 0.0
    stalls = 0
    n = 0
    while True:
        pid = viewport_pid()
        if pid is None:
            time.sleep(1)
            continue

        # The probe's own timeout has to exceed the threshold, or a long stall
        # is recorded as a failure rather than measured.
        took = probe(path, max(args.threshold * 4, 10.0))
        n += 1
        if took is None:
            stalls += 1
            print("  no answer within timeout — capturing", flush=True)
            for i in range(3):
                capture(gdb, pid, out, "unresponsive, sample %d" % (i + 1))
                time.sleep(1.5)
        elif took > args.threshold:
            stalls += 1
            worst = max(worst, took)
            print("  STALL %.2fs — capturing" % took, flush=True)
            capture(gdb, pid, out, "loop blocked %.2fs" % took)
        else:
            worst = max(worst, took)

        if n % 40 == 0:
            print("  %d probes, %d stalls, worst %.2fs" % (n, stalls, worst),
                  flush=True)
        time.sleep(0.5)


if __name__ == "__main__":
    main()
