#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# What the shell costs when it is drawing, and what the whole thing weighs.
#
# `bench-vkcube.sh` measures the compositor: a fullscreen client, an idle
# desktop behind it, and four backends that come out identical because an
# engine that has painted its desktop and been given no reason to repaint costs
# the same whichever engine it is. This measures the other half — the shell
# doing work — and it measures it across every process the desktop is made of
# rather than only the compositor's own.
#
# Two numbers, and both of them needed saying differently from the other
# harness:
#
#   CPU     summed over the compositor and every process it started, because
#           the engine lives inside the compositor for `wpe` and in a process
#           of its own for the other three. A per-process number cannot
#           compare across that line.
#
#   Memory  PSS, not RSS, summed over the same tree. RSS counts a shared
#           library once per process that maps it, and CEF and Chromium run
#           four or five processes that share most of an engine — adding their
#           RSS together reports a desktop twice the size of the machine's
#           actual commitment. PSS divides each shared page by the number of
#           processes sharing it, which is the number that adds up.
#
# The load is the shell's own commands, over the control socket, at a fixed
# cadence: the overview draws a miniature of every window, a workspace switch
# re-lays-out the whole desktop, and a resize delta is what dragging an edge
# does sixty times a second. All three are the shell repainting, which is the
# thing being measured.

import argparse
import json
import os
import pathlib
import signal
import socket
import statistics
import subprocess
import sys
import time

# The load, one command per tick, repeated. Ordered so the expensive one is not
# the only one: a desktop that only ever draws miniatures is not a desktop.
LOAD = [
    ("layout.overview", []),  # on
    ("workspace.switch", ["2"]),
    ("layout.resize.delta", ["20"]),
    ("layout.resize.delta", ["-20"]),
    ("layout.overview", []),  # off
    ("workspace.switch", ["1"]),
    ("layout.resize.delta", ["20"]),
    ("layout.resize.delta", ["-20"]),
]


def clock_ticks() -> int:
    return os.sysconf("SC_CLK_TCK")


def tree(root: int) -> list[int]:
    """Every process descended from `root`, including it.

    Read fresh each sample: Chromium starts its zygote, its GPU process and a
    renderer after the window is up, and a tree taken once at the start would
    miss most of the engine.
    """
    children: dict[int, list[int]] = {}
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / "stat").read_text()
        except OSError:
            continue
        # The comm field is parenthesised and may contain spaces, so the parse
        # starts after the last ')'.
        rest = stat[stat.rfind(")") + 2 :].split()
        parent = int(rest[1])
        children.setdefault(parent, []).append(int(entry.name))

    found, queue = [], [root]
    while queue:
        pid = queue.pop()
        found.append(pid)
        queue.extend(children.get(pid, []))
    return found


def cpu_jiffies(pids: list[int]) -> int:
    total = 0
    for pid in pids:
        try:
            stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
        except OSError:
            continue
        rest = stat[stat.rfind(")") + 2 :].split()
        # utime and stime, and the children fields deliberately not: a process
        # that has exited is gone from the tree and counting it here would make
        # the total depend on when a renderer was restarted.
        total += int(rest[11]) + int(rest[12])
    return total


def pss_kb(pids: list[int]) -> int:
    total = 0
    for pid in pids:
        try:
            rollup = pathlib.Path(f"/proc/{pid}/smaps_rollup").read_text()
        except OSError:
            continue
        for line in rollup.splitlines():
            if line.startswith("Pss:"):
                total += int(line.split()[1])
                break
    return total


class Socket:
    """The control socket, which is how the shell is driven."""

    def __init__(self, path: str):
        self.path = path
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.setblocking(False)

    def send(self, message: dict) -> None:
        payload = (json.dumps(message) + "\n").encode()
        try:
            self.sock.sendall(payload)
        except BlockingIOError:
            # The compositor is behind. Dropping a tick of load is better than
            # blocking the sampler, which would show up as a gap in the CPU
            # series and read as the desktop going quiet.
            pass

    def drain(self) -> None:
        try:
            while self.sock.recv(65536):
                pass
        except (BlockingIOError, OSError):
            pass


def wait_for(log: pathlib.Path, needle: str, timeout: float) -> str | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            text = log.read_text(errors="replace")
        except OSError:
            text = ""
        for line in text.splitlines():
            if needle in line:
                return line
        time.sleep(0.1)
    return None


def run(binary: str, out: pathlib.Path, seconds: float, clients: int, client_bin: str):
    out.mkdir(parents=True, exist_ok=True)
    spawned: list[subprocess.Popen] = []
    log = out / "viewport.log"
    handle = log.open("w")

    # `VIEWPORT_SHELL_RATE` makes the compositor say how fast the shell is
    # painting, once a second. Without it the log has a frame *total* and no
    # rate, and a shell producing four frames a second looks like one producing
    # sixty.
    environment = dict(os.environ, RUST_LOG="info", VIEWPORT_SHELL_RATE="1")
    compositor = subprocess.Popen(
        [binary, "--drm"],
        stdout=handle,
        stderr=subprocess.STDOUT,
        env=environment,
    )

    try:
        line = wait_for(log, "WAYLAND_DISPLAY=", 60)
        if line is None:
            raise SystemExit("the compositor never announced a socket")
        display = line.split("WAYLAND_DISPLAY=")[1].split()[0]

        # The shell talking is the point at which the desktop exists. Without
        # waiting for it the load would be sent to a page that cannot receive
        # it, and the run would measure a compositor drawing nothing.
        if wait_for(log, "shell is talking to us", 60) is None:
            raise SystemExit("the shell never spoke; there is nothing to measure")

        runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
        control = Socket(f"{runtime}/viewport-{display}.sock")

        # Windows for the shell to lay out. An empty desktop has nothing to
        # re-lay-out and the overview draws no miniatures, so the load would be
        # a page redrawing its wallpaper.
        for _ in range(clients):
            spawned.append(
                subprocess.Popen(
                    [client_bin, "-e", "sh", "-c", "while :; do sleep 5; done"],
                    env=dict(environment, WAYLAND_DISPLAY=display),
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            )
        time.sleep(4)

        # A moment of quiet first: this is what the desktop costs when nobody
        # is touching it, and it is the baseline the loaded figure is only
        # meaningful against.
        pids = tree(compositor.pid)
        idle_start, idle_at = cpu_jiffies(pids), time.monotonic()
        idle_pss = []
        while time.monotonic() - idle_at < 3:
            idle_pss.append(pss_kb(tree(compositor.pid)))
            time.sleep(0.25)
        idle_cpu = (cpu_jiffies(tree(compositor.pid)) - idle_start) / clock_ticks()
        idle_seconds = time.monotonic() - idle_at

        # And the load.
        started = cpu_jiffies(tree(compositor.pid))
        started_compositor = cpu_jiffies([compositor.pid])
        at = time.monotonic()
        samples, tick = [], 0
        while time.monotonic() - at < seconds:
            command, args = LOAD[tick % len(LOAD)]
            control.send({"type": "shell.command", "command": command, "args": args})
            control.drain()
            tick += 1
            pids = tree(compositor.pid)
            samples.append((pss_kb(pids), len(pids)))
            time.sleep(0.25)
        elapsed = time.monotonic() - at
        busy = (cpu_jiffies(tree(compositor.pid)) - started) / clock_ticks()

        # The shell's own paint rate, out of the compositor's log. Read after
        # the load rather than sampled during it: the line is emitted once a
        # second by the compositor's own housekeeping tick, so the file already
        # holds the series.
        rates = []
        for line in log.read_text(errors="replace").splitlines():
            if "shell: " in line and "frames/s" in line:
                try:
                    rates.append(float(line.split("shell: ")[1].split(" frames/s")[0]))
                except (IndexError, ValueError):
                    continue
        # The last `seconds` worth, which is the loaded part of the run: the
        # ticks before it are the idle baseline and would drag the median down.
        loaded = rates[-int(elapsed) :] if rates else []

        # How *evenly* it painted, which is a different question from how much.
        #
        # The load alternates between an overview that repaints hard and a
        # desktop that repaints twice a second, so a median over the whole
        # window measures the mixture rather than either state. What a person
        # calls smooth is the animating state holding a steady cadence — a
        # backend that swings between 10 and 89 frames a second reads as stutter
        # even though it painted more frames than one that holds 26. That is not
        # hypothetical: it is `cef` against `servoshell` on this machine, and it
        # is why these two columns exist. Ticks under five frames a second are
        # the idle half of the cycle and are left out.
        animating = [rate for rate in loaded if rate >= 5]
        mean_rate = (sum(animating) / len(animating)) if animating else 0.0
        spread = (
            statistics.pstdev(animating) if len(animating) > 1 else 0.0
        )

        # The compositor's own share of it, for the one comparison that is not
        # about the engine: how much of the desktop is inside the compositor.
        # A delta over the same window, not the process's total since it
        # started — which would count the whole of startup and read as the
        # compositor being busier than the engine.
        compositor_only = (
            cpu_jiffies([compositor.pid]) - started_compositor
        ) / clock_ticks()
        compositor_pss = pss_kb([compositor.pid])

        control.send({"type": "quit"})
        result = {
            "idle_cpu_pct": 100 * idle_cpu / idle_seconds,
            "idle_pss_mb": (sum(idle_pss) / len(idle_pss)) / 1024,
            "load_cpu_pct": 100 * busy / elapsed,
            "load_pss_mb": (sum(p for p, _ in samples) / len(samples)) / 1024,
            "peak_pss_mb": max(p for p, _ in samples) / 1024,
            "compositor_pss_mb": compositor_pss / 1024,
            "compositor_cpu_pct": 100 * compositor_only / elapsed,
            "shell_fps": (sorted(loaded)[len(loaded) // 2] if loaded else 0.0),
            "shell_fps_peak": (max(loaded) if loaded else 0.0),
            # While animating: the rate, how far it wanders, and the two as a
            # ratio. `shell_fps_cov` is the one to read — a rate is only smooth
            # relative to itself.
            "shell_fps_animating": mean_rate,
            "shell_fps_spread": spread,
            "shell_fps_cov": (spread / mean_rate) if mean_rate else 0.0,
            "shell_fps_floor": (min(animating) if animating else 0.0),
            "processes": max(n for _, n in samples),
            "commands": tick,
            "seconds": elapsed,
        }
        (out / "shell-load.json").write_text(json.dumps(result, indent=2))
        return result
    finally:
        for client in spawned:
            client.terminate()
        if compositor.poll() is None:
            compositor.send_signal(signal.SIGTERM)
        try:
            compositor.wait(timeout=15)
        except subprocess.TimeoutExpired:
            compositor.kill()
        handle.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--viewport", required=True, help="the compositor to run")
    parser.add_argument("--out", required=True, help="where to write the result")
    parser.add_argument("--seconds", type=float, default=20.0)
    parser.add_argument("--clients", type=int, default=4)
    parser.add_argument("--client-bin", default="foot")
    args = parser.parse_args()

    result = run(
        args.viewport,
        pathlib.Path(args.out),
        args.seconds,
        args.clients,
        args.client_bin,
    )
    print(json.dumps(result), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
