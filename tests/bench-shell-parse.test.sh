#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# The one thing scripts/bench-shell.py reads out of the compositor's log.
#
# The pacing line is a contract between a `tracing::info!` in the compositor and
# a harness that can be many versions away from it, so it is pinned here rather
# than discovered on a TTY: a fixture line carrying every field, a line with no
# pacing half at all — which is what every compositor older than the metric
# wrote — a window the shell barely painted in, and a tail that has grown
# something the harness does not know. None of them may raise; the first has to
# produce the numbers `docs/benchmarks.md` quotes, and the rest have to degrade
# to what the harness recorded before the columns existed.
#
# No compositor, no session, no seat: this is the parse, not the run.
#
#   tests/bench-shell-parse.test.sh
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if ! command -v python3 >/dev/null; then
	echo "SKIP: no python3, and the harness is a python script" >&2
	exit 77
fi

python3 - "$root" <<'PY'
import importlib.util
import math
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
harness = root / "scripts/bench-shell.py"
spec = importlib.util.spec_from_file_location("bench_shell", harness)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.PACING_MARKER == "shell-pacing:", module.PACING_MARKER

# A line exactly as `ViewportState::report_shell_rate` writes it, journald prefix
# and all: 60 frames in a second, one gap of 301.2 ms in the middle of them.
FIXTURE = (
    "2026-11-24T22:00:00.000000Z  INFO viewport: shell: 59.9 frames/s (60 in 1.00s) "
    "shell-pacing: window_ms=1000 count=60 period_ms=16.67 gap_p50_ms=16.61 "
    "gap_p95_ms=17.02 gap_max_ms=301.20 hitches=1 stalls=1"
)
fields = module.parse_pacing(FIXTURE)
assert fields is not None, "the fixture line did not parse"
expected = {
    "window_ms": 1000.0,
    "count": 60.0,
    "period_ms": 16.67,
    "gap_p50_ms": 16.61,
    "gap_p95_ms": 17.02,
    "gap_max_ms": 301.20,
    "hitches": 1.0,
    "stalls": 1.0,
}
for key, value in expected.items():
    assert key in fields, f"the fixture lost {key}"
    assert math.isclose(fields[key], value, abs_tol=1e-9), (key, fields[key], value)

# The rate the human half carries is still readable, which is what makes the
# `docs/debugging.md` advice survive the change.
rate = float(FIXTURE.split("shell: ")[1].split(" frames/s")[0])
assert rate == 59.9, rate

# A line from a compositor that has no metric: the rate and nothing else. The
# harness reads it for `shell_fps` and records no gaps, which is what it did
# before the columns existed.
assert module.parse_pacing("INFO viewport: shell: 26.0 frames/s (26 in 1.00s)") is None

# A window in which the shell painted fewer than twice: the compositor reports
# the count and no gap statistics at all, and the harness has to read that as
# "no gaps" rather than as a zero.
sparse = module.parse_pacing(
    "INFO viewport: shell: 1.0 frames/s (1 in 1.00s) shell-pacing: "
    "window_ms=1000 count=1 period_ms=16.67"
)
assert sparse is not None
assert sparse["count"] == 1.0
assert "gap_p50_ms" not in sparse, "a skipped window must not look like a perfect one"

# Something appended that the harness was never taught: an unknown key is
# carried and ignored by every reader, and a token that is not `key=value` at
# all is dropped rather than raised on, so a newer compositor cannot break an
# older harness.
grown = module.parse_pacing(
    "shell-pacing: window_ms=1000 count=60 gap_p50_ms=16.61 newfield=3 hello"
)
assert grown is not None
assert grown["count"] == 60.0, grown
assert grown["newfield"] == 3.0, grown
assert "hello" not in grown, grown

# The other half of the contract: the fields the harness reads are the fields
# the compositor writes. Both sides are in this tree, so a rename that only
# touches one of them is caught here rather than on a TTY.
source = (root / "crates/viewport/src/state.rs").read_text()
assert module.PACING_MARKER in source, "the compositor stopped writing the marker"
for key in expected:
    assert f"{key}=" in source, f"the compositor no longer writes {key}="

print("bench-shell parsing: the fixture, the fallback and the contract all hold")
PY
