#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Multi-device SRQ tests: does the service-request path behave on a real bus?

Requires two instruments on the SAME adapter's bus at different primary
addresses, and no second GPIB controller attached -- two system controllers
will fight over IFC and ATN.

    ./multidev_srq.py --a 23 --b 3

PAD A is the device under test, PAD B the other one. Both are probed for
whether they can raise SRQ at all before any daemon behaviour is judged, so an
instrument that cannot signal RQS is reported as such rather than being
mistaken for a daemon fault.

This is worth running even when a single-instrument bus looks healthy. A bus
with one device cannot show addressing faults at all: it was this script that
caught writes being delivered to every previously-addressed listener, because
a command sent to one instrument was executed by the other.

Note that a serial poll *clears* RQS, and the daemon polls the same device
concurrently to service the request. Tests here therefore ask for status with
*STB?, which reports the same summary without consuming it; read_stb() would
race the daemon and intermittently report RQS clear on an instrument that did
request service.

Requires a pyvisa-py whose HiSLIP backend implements service-request events.

Exit code 0 if every test passed, 1 otherwise.
"""
from __future__ import annotations

import argparse
import time

import pyvisa

RQS = 0x40
SRQ_EVENT = pyvisa.constants.EventType.service_request
QUEUE = pyvisa.constants.EventMechanism.queue

rm = pyvisa.ResourceManager("@py")
results: list[tuple[str, bool, str]] = []


def report(name: str, ok: bool, detail: str) -> None:
    print(f"{'PASS' if ok else 'FAIL'}  {name}  {detail}")
    results.append((name, ok, detail))


def open_pad(pad: int, timeout: int = 15000):
    inst = rm.open_resource(f"TCPIP::127.0.0.1::hislip{pad}::INSTR", open_timeout=timeout)
    inst.timeout = timeout
    return inst


def arm(inst) -> None:
    """Ask the instrument to assert SRQ on a command error (IEEE-488.2)."""
    inst.write("*CLS")
    inst.write("*ESE 32")   # command error -> ESB
    inst.write("*SRE 32")   # ESB -> SRQ
    time.sleep(0.1)


def disarm(inst) -> None:
    inst.write("*SRE 0")
    inst.write("*CLS")
    time.sleep(0.05)


def provoke(inst) -> None:
    inst.write("BOGUSCMD")


def poll_until_rqs(inst, seconds: float = 2.0) -> int:
    """Wait for the instrument to report that it is requesting service.

    Uses *STB? rather than read_stb(): a serial poll *clears* RQS, and the
    daemon's SRQ forwarder is polling the same device concurrently. Whichever
    poll lands first consumes the bit, so read_stb() here races the forwarder
    and intermittently reports RQS clear on an instrument that did request
    service. *STB? reports MSS without clearing it, so both can observe it.

    Writes are handshaken before they are parsed, so keep asking rather than
    trusting a single read.
    """
    stb = 0
    deadline = time.time() + seconds
    while time.time() < deadline:
        stb = int(inst.query("*STB?").strip())
        if stb & RQS:
            return stb
        time.sleep(0.05)
    return stb


def drain(inst, settle: float = 0.4) -> int:
    """Count and clear queued service-request events.

    Settles first: a service request the daemon has already sent may still be
    in the socket buffer, and counting it against the *next* phase would look
    like a leak. Draining without a settle races delivery.
    """
    time.sleep(settle)
    n = 0
    while True:
        try:
            inst.wait_on_event(SRQ_EVENT, 100)
            n += 1
        except pyvisa.errors.VisaIOError:
            return n


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--a", type=int, required=True, help="PAD of device A")
    ap.add_argument("--b", type=int, required=True, help="PAD of device B")
    args = ap.parse_args()

    a, b = open_pad(args.a), open_pad(args.b)
    try:
        print(f"A = PAD {args.a}: {a.query('*IDN?').strip()}")
        print(f"B = PAD {args.b}: {b.query('*IDN?').strip()}\n")

        # Listen from the very start. Enabling events only later would leave
        # earlier service requests sitting in the socket, to be dispatched the
        # moment the monitor starts and counted against whatever phase is
        # running then.
        a.enable_event(SRQ_EVENT, QUEUE)
        b.enable_event(SRQ_EVENT, QUEUE)

        # --- 1. Can each device actually raise SRQ *and* set RQS? ----------
        # This is a property of the instrument, not the daemon. If RQS is
        # never set, the forwarder is right to drop it and the instrument
        # simply cannot be served this way -- report that, don't blame the
        # daemon for the tests below.
        capable = {}
        for name, inst in (("A", a), ("B", b)):
            arm(inst)
            provoke(inst)
            stb = poll_until_rqs(inst)
            capable[name] = bool(stb & RQS)
            report(
                f"{name} sets RQS on SRQ",
                capable[name],
                f"STB=0x{stb:02x} (RQS {'set' if stb & RQS else 'NOT set'})",
            )
            disarm(inst)

        if not capable["B"]:
            print("\nB cannot signal RQS; the wired-OR test below is not meaningful.")
            return 1

        # --- 2. B raises SRQ: A's session must NOT get an event ------------
        # This is the case the RQS filter exists for and the one with no
        # hardware evidence so far.
        drain(a)
        drain(b)
        arm(b)
        provoke(b)

        got_b = False
        try:
            b.wait_on_event(SRQ_EVENT, 5000)
            got_b = True
        except pyvisa.errors.VisaIOError:
            pass
        leaked_a = drain(a)

        report("B's SRQ reaches B", got_b, "service_request delivered on B's session")
        report(
            "B's SRQ does NOT leak to A",
            leaked_a == 0,
            f"{leaked_a} spurious event(s) on A" if leaked_a else "A saw nothing, correct",
        )
        disarm(b)

        # --- 3. A raises SRQ: A gets it, B does not -----------------------
        drain(a)
        drain(b)
        arm(a)
        provoke(a)
        got_a = False
        try:
            a.wait_on_event(SRQ_EVENT, 5000)
            got_a = True
        except pyvisa.errors.VisaIOError:
            pass
        leaked_b = drain(b)
        report("A's SRQ reaches A", got_a, "service_request delivered on A's session")
        report(
            "A's SRQ does NOT leak to B",
            leaked_b == 0,
            f"{leaked_b} spurious event(s) on B" if leaked_b else "B saw nothing, correct",
        )
        disarm(a)

        # --- 4. Both at once: each session gets its own ---------------------
        drain(a)
        drain(b)
        arm(a)
        arm(b)
        provoke(a)
        provoke(b)
        both = []
        for name, inst in (("A", a), ("B", b)):
            try:
                inst.wait_on_event(SRQ_EVENT, 5000)
                both.append(name)
            except pyvisa.errors.VisaIOError:
                pass
        report(
            "simultaneous SRQ reaches both",
            both == ["A", "B"],
            f"delivered to {both or 'neither'}",
        )
        disarm(a)
        disarm(b)

        a.disable_event(SRQ_EVENT, QUEUE)
        b.disable_event(SRQ_EVENT, QUEUE)

        # --- 5. both devices still work afterwards -------------------------
        for name, inst in (("A", a), ("B", b)):
            idn = inst.query("*IDN?").strip()
            report(f"{name} healthy after SRQ storm", bool(idn), idn or "no response")
    finally:
        a.close()
        b.close()

    print()
    passed = sum(1 for _, ok, _ in results if ok)
    print(f"{passed}/{len(results)} passed")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
