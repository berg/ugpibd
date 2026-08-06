#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Classify how an instrument emits bulk data, using only what ugpibd ships today.

Groundwork for docs/CAPTURE.md. That design needs a new backend primitive
(unaddressed listen), but three of its open questions can be answered with the
current `read` path and no new Rust at all:

  1. Does a talk-only instrument's output reach us through an ordinary read?
     `read()` sends UNL, MLA(us), MTA(pad), then drops to standby. An
     instrument in talk-only drives the bus regardless of addressing, and we
     are an addressed listener, so it may already work. If it does, talk-only
     logging is nearly free.

  2. Does a talk-only instrument monopolise the bus? KE5FX warns that some do.
     With a second instrument present this is directly checkable rather than
     a rumour to repeat.

  3. Is a plotting instrument case 1/2 (dumps and never looks back) or case
     3/4 (expects a plotter that answers)? The captured bytes say which: an
     instrument that emits `OI`, `OE`, `OP` or `OS` is querying a plotter and
     will not be satisfied by anything that only listens.

Question 3 is the one that decides whether the expensive half of CAPTURE.md
gets built, and it needs no device-mode support to answer.

    ./bus_capture_probe.py --talk-only-pad 23 --other-pad 3
    ./bus_capture_probe.py --plot-pad 3 --capture-file plot.hpgl

Requires pyvisa and a running ugpibd. See docs/HARDWARE-TEST.md for the
front-panel steps each test expects.

Exit code 0 if every test that ran reached a conclusion, 1 otherwise. A test
that cannot distinguish its outcomes reports INCONCLUSIVE and fails, rather
than guessing -- an inconclusive capture looks exactly like a quiet bus.
"""

from __future__ import annotations

import argparse
import sys
import time

try:
    import pyvisa
except ImportError:
    print("pyvisa is required: pip install pyvisa pyvisa-py", file=sys.stderr)
    raise SystemExit(2)


# HP-GL instructions that ask the plotter a question. Their presence in a
# captured stream means the instrument expects an answer, i.e. a passive
# listener is not enough (CAPTURE.md cases 3/4).
HPGL_QUERIES = (b"OI", b"OE", b"OP", b"OS", b"OA", b"OC", b"OF", b"OH", b"OO")

# Instructions any real HP-GL stream contains. Used only to tell "we captured
# a plot" from "we captured noise".
HPGL_MARKERS = (b"IN;", b"SP", b"PU", b"PD", b"PA", b"DF;", b"LB")


def open_instr(rm, host: str, pad: int, timeout_ms: int):
    resource = f"TCPIP::{host}::hislip{pad}::INSTR"
    inst = rm.open_resource(resource, open_timeout=timeout_ms)
    inst.timeout = timeout_ms
    return inst


def drain(inst, budget_s: float) -> tuple[bytes, list[float]]:
    """Read until the source goes quiet or the budget expires.

    Returns the bytes and the wall-clock gap before each chunk. The gaps are
    what a client-side inactivity threshold should be chosen from.

    Uses `read_raw()`, not `read_bytes(n)`. `read_bytes` reads *exactly* n
    bytes or raises, and pyvisa discards the partial buffer on timeout — so a
    plot whose length is not a multiple of the chunk size loses its tail, which
    is the single worst failure mode for this script (a truncated capture looks
    like a successful one). `read_raw()` reads until END instead, and the daemon
    terminates every GPIB read, so one call is one read's worth of data.
    """
    out = bytearray()
    gaps: list[float] = []
    deadline = time.monotonic() + budget_s
    last = time.monotonic()
    while time.monotonic() < deadline:
        try:
            piece = inst.read_raw()
        except Exception:
            # A timeout here is the normal way a quiet bus reports itself.
            break
        if not piece:
            break
        now = time.monotonic()
        gaps.append(now - last)
        last = now
        out.extend(piece)
    return bytes(out), gaps


def summarize(data: bytes) -> str:
    printable = sum(1 for b in data if 0x20 <= b < 0x7F or b in (10, 13))
    ratio = printable / len(data) if data else 0.0
    kind = "text" if ratio > 0.95 else "binary/mixed"
    return f"{len(data)} bytes, {ratio:.0%} printable ({kind})"


def test_talk_only(host: str, pad: int, budget_s: float, timeout_ms: int) -> bool:
    """Does an ordinary read pick up a talk-only instrument's output?

    Control arm: the operator is asked to take the instrument *out* of
    talk-only and the same read is repeated. Without that arm a test that
    always returns bytes -- because the instrument answers addressing
    normally -- would look like success.
    """
    print(f"\n=== talk-only source at pad {pad} ===")
    print("Put the instrument in TALK ONLY from the front panel, then press Enter.")
    input()

    rm = pyvisa.ResourceManager()
    try:
        inst = open_instr(rm, host, pad, timeout_ms)
    except Exception as e:
        print(f"  FAIL: could not open instrument: {e}")
        return False

    data, gaps = drain(inst, budget_s)
    print(f"  talk-only:  {summarize(data)}")
    if data:
        print(f"  first line: {data.splitlines()[0][:70]!r}")

    print("\nNow take the instrument OUT of talk-only (normal addressed mode),")
    print("then press Enter for the control arm.")
    input()
    control, _ = drain(inst, min(budget_s, 5.0))
    print(f"  control:    {summarize(control)}")
    inst.close()

    if not data:
        print("  RESULT: no bytes in talk-only. An ordinary read does not reach")
        print("          a talk-only source; unaddressed listen is required.")
        return True
    if len(control) >= len(data):
        print("  INCONCLUSIVE: the control arm returned as much as the test arm,")
        print("          so this does not show talk-only had any effect.")
        return False
    print("  RESULT: talk-only output reaches us through the existing read path.")
    print("          Talk-only logging needs no new backend primitive.")
    return True


def test_bus_monopoly(host: str, talk_pad: int, other_pad: int, timeout_ms: int) -> bool:
    """With one instrument in talk-only, can we still talk to another?"""
    print(f"\n=== bus monopoly: pad {talk_pad} talk-only, querying pad {other_pad} ===")
    print(f"Put pad {talk_pad} in TALK ONLY, leave pad {other_pad} normal, press Enter.")
    input()

    rm = pyvisa.ResourceManager()
    try:
        other = open_instr(rm, host, other_pad, timeout_ms)
        idn = other.query("*IDN?").strip()
        other.close()
    except Exception as e:
        print(f"  RESULT: pad {other_pad} unreachable while pad {talk_pad} talks ({e})")
        print("          Talk-only monopolises this bus. Document it as a")
        print("          consequence of the mode, not as a daemon fault.")
        return True
    print(f"  RESULT: pad {other_pad} still answers: {idn[:60]!r}")
    print("          Talk-only coexists with normal traffic on this bus.")
    return True


def test_plot(host: str, pad: int, budget_s: float, timeout_ms: int,
              capture_file: str | None, plot_cmd: str | None) -> bool:
    """Capture a plot and decide whether the instrument expects a real plotter."""
    print(f"\n=== plot capture at pad {pad} ===")
    rm = pyvisa.ResourceManager()
    try:
        inst = open_instr(rm, host, pad, timeout_ms)
    except Exception as e:
        print(f"  FAIL: could not open instrument: {e}")
        return False

    if plot_cmd:
        print(f"  sending {plot_cmd!r}")
        inst.write(plot_cmd)
    else:
        print("  Press PLOT on the front panel, then press Enter.")
        input()

    data, gaps = drain(inst, budget_s)
    inst.close()
    print(f"  captured:   {summarize(data)}")

    if not data:
        print("  RESULT: nothing captured. Either the instrument addresses the")
        print("          plotter itself (case 3/4) or it needs an unaddressed")
        print("          listener. Re-run the diagnose step once bus_lines()")
        print("          exists and watch ATN.")
        return True

    if capture_file:
        with open(capture_file, "wb") as f:
            f.write(data)
        print(f"  written:    {capture_file}")

    looks_hpgl = any(m in data for m in HPGL_MARKERS)
    print(f"  HP-GL:      {'yes' if looks_hpgl else 'no recognizable instructions'}")

    found = sorted({q.decode() for q in HPGL_QUERIES if q + b";" in data or q + b"\n" in data})
    if found:
        print(f"  queries:    {', '.join(found)}")
        print("  RESULT: the instrument QUERIES the plotter. A passive listener")
        print("          will not satisfy it -- this is CAPTURE.md case 3/4 and")
        print("          needs device mode plus a plotter persona.")
    else:
        print("  queries:    none")
        print("  RESULT: the instrument dumps and never looks back. Passive")
        print("          capture is sufficient -- CAPTURE.md case 1/2.")

    if gaps:
        longest = max(gaps)
        print(f"  framing:    {len(gaps)} chunks, longest inter-chunk gap {longest*1000:.0f} ms")
        print(f"              suggests --until idle:{int(longest * 3000)}ms as a floor")
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--timeout-ms", type=int, default=10000)
    ap.add_argument("--budget", type=float, default=20.0,
                    help="seconds to keep reading before giving up (default 20)")
    ap.add_argument("--talk-only-pad", type=int,
                    help="instrument to place in talk-only (34401A, 53132A, ...)")
    ap.add_argument("--other-pad", type=int,
                    help="second instrument, for the bus-monopoly check")
    ap.add_argument("--plot-pad", type=int,
                    help="plotting instrument (SR620, 53310A, ...)")
    ap.add_argument("--plot-cmd",
                    help="command that starts a plot; omit to use the front panel")
    ap.add_argument("--capture-file", help="write captured plot bytes here")
    args = ap.parse_args()

    if not (args.talk_only_pad or args.plot_pad):
        ap.error("give at least one of --talk-only-pad or --plot-pad")

    ok = True
    if args.talk_only_pad:
        ok &= test_talk_only(args.host, args.talk_only_pad, args.budget, args.timeout_ms)
        if args.other_pad:
            ok &= test_bus_monopoly(args.host, args.talk_only_pad, args.other_pad,
                                    args.timeout_ms)
    if args.plot_pad:
        ok &= test_plot(args.host, args.plot_pad, args.budget, args.timeout_ms,
                        args.capture_file, args.plot_cmd)

    print("\nNot covered here, because they need the CAPTURE.md primitive:")
    print("  * unaddressed listen (AUX_LON|AUX_CS) -- true listen-only capture")
    print("  * bus_lines() -- watching ATN to separate case 2 from case 3")
    print("  * clearing SYSTEM_CONTROLLER -- whether the read engine runs")
    print("    while not controller-in-charge")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
