#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The shell doing work, once per backend, with every process counted.
#
#   ./scripts/bench-shell.sh                  # every implemented backend
#   ./scripts/bench-shell.sh --only cef
#   ./scripts/bench-shell.sh --seconds 40
#
# `bench-backends.sh` runs the vkcube harness per backend and finds them
# identical, because that harness measures a compositor with a fullscreen
# client over an idle desktop. This one drives the shell — the overview, a
# workspace switch, a resize delta, at four commands a second over the control
# socket — and samples CPU and PSS across the compositor *and* every process it
# started. See scripts/bench-shell.py for why PSS and why the whole tree.
#
# From a TTY, for the same reason as the other one: the shell paints against a
# real output at a real refresh rate, and nested it paints against the host's.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

backends=(wpe webkitgtk chromium cef)
seconds=20
clients=4
runs=3
stamp=$(date +%Y%m%d-%H%M%S)
outroot="$here/bench-results/shell-$stamp"

while [ $# -gt 0 ]; do
    case "$1" in
        --only) backends=("$2"); shift 2 ;;
        --seconds) seconds=$2; shift 2 ;;
        --clients) clients=$2; shift 2 ;;
        --runs) runs=$2; shift 2 ;;
        --out) outroot=$2; shift 2 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

# The session that is active on the seat, which is the one libseat will let
# take DRM master — see the same block in bench-backends.sh for why this is
# asked of the seat rather than worked out from session states.
if [ -z "${XDG_VTNR:-}" ] || [ -z "${XDG_SESSION_ID:-}" ]; then
    session=$(loginctl show-seat seat0 -p ActiveSession --value 2>/dev/null || true)
    if [ -z "$session" ]; then
        echo "no active session on seat0; switch to a TTY and log in" >&2
        exit 1
    fi
    vt=$(loginctl show-session "$session" -p VTNr --value 2>/dev/null || true)
    export XDG_SESSION_ID=$session XDG_VTNR=$vt
    echo "using session $session on tty$vt" >&2
fi

# The windows the shell lays out. Not a dependency of the compositor, so it is
# found on PATH or fetched — the same rule bench-vkcube.sh uses for vkcube.
client_bin=$(command -v foot || true)
if [ -z "$client_bin" ]; then
    echo "fetching a terminal for the windows..." >&2
    client_bin=$(nix build --no-link --print-out-paths nixpkgs#foot)/bin/foot
fi

declare -A binary
for backend in "${backends[@]}"; do
    echo "building .#$backend (nothing is on screen until every build is done)..." >&2
    binary[$backend]=$(nix build "$here#$backend" --no-link --print-out-paths)/bin/viewport
done

mkdir -p "$outroot"
for backend in "${backends[@]}"; do
    echo >&2
    echo "=== $backend ===" >&2
    # Repeated, because the vkcube runs taught this the hard way: session CPU
    # varied more between runs of one backend than between backends, and a
    # single run of anything here would have been noise reported as a finding.
    : >"$outroot/$backend.jsonl"
    for run in $(seq 1 "$runs"); do
        echo "  run $run/$runs" >&2
        python3 "$here/scripts/bench-shell.py" \
            --viewport "${binary[$backend]}" \
            --out "$outroot/$backend/run$run" \
            --seconds "$seconds" \
            --clients "$clients" \
            --client-bin "$client_bin" >>"$outroot/$backend.jsonl"
    done
    cat "$outroot/$backend.jsonl" >&2
done

echo >&2
echo "results under $outroot" >&2
python3 - "$outroot" "${backends[@]}" <<'PY' | tee "$outroot/summary.md"
import json, pathlib, statistics, sys

root, backends = pathlib.Path(sys.argv[1]), sys.argv[2:]
print("# The shell under load, whole desktop counted\n")
print("| backend | idle cpu % | load cpu % | of which compositor % | shell fps "
      "| idle pss MB | load pss MB | peak pss MB | processes |")
print("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
for backend in backends:
    path = root / f"{backend}.jsonl"
    if not path.exists():
        continue
    runs = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    if not runs:
        continue
    # The median of each column independently. Not one representative run: the
    # columns are measuring different things and the middle run of one is not
    # the middle run of another.
    r = {key: statistics.median(run[key] for run in runs) for key in runs[0]}
    print(
        f"| {backend} | {r['idle_cpu_pct']:.1f} | {r['load_cpu_pct']:.1f} "
        f"| {r['compositor_cpu_pct']:.1f} | {r.get('shell_fps', 0):.1f} "
        f"| {r['idle_pss_mb']:.0f} | {r['load_pss_mb']:.0f} "
        f"| {r['peak_pss_mb']:.0f} | {r['processes']} |"
    )
print("\nMedian of every run. CPU is the compositor and every process it started,")
print("over the run. Memory is PSS across the same tree: RSS would count a shared")
print("engine once per process that maps it, which for CEF and Chromium is four or")
print("five times.\n")
print("## Every run\n")
print("| backend | load cpu % | load pss MB | shell fps |")
print("| --- | --- | --- | --- |")
for backend in backends:
    path = root / f"{backend}.jsonl"
    if not path.exists():
        continue
    runs = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    cpu = ", ".join(f"{run['load_cpu_pct']:.1f}" for run in runs)
    pss = ", ".join(f"{run['load_pss_mb']:.0f}" for run in runs)
    fps = ", ".join(f"{run.get('shell_fps', 0):.0f}" for run in runs)
    print(f"| {backend} | {cpu} | {pss} | {fps} |")
PY
