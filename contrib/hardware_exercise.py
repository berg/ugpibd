#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Exercise a running ugpibd against one real instrument over HiSLIP.

A deeper counterpart to smoke_test.py: that one proves a round trip works,
this one exercises the operations that have historically been broken in ways
a round trip cannot see -- serial poll, device clear, GPIB trigger, service
requests, and bus recovery after a timeout.

    ./hardware_exercise.py --pad 23

The measurement tests assume an HP/Agilent 34401A. Everything else is generic.

Requires pyvisa, and a pyvisa-py whose HiSLIP backend implements read_stb,
assert_trigger and service-request events -- the released version implements
none of the three, and those tests will report VI_ERROR_NSUP_OPER.

Two rules this file tries to follow, both learned the hard way:

* A test must be able to fail. Checking that an instrument still answers
  after a device clear passes even when the clear does nothing, which is how
  a mis-addressed SDC survived. Prove the effect, and include a control arm
  showing the probe can fail.

* A GPIB write completes when its bytes are handshaken, not when the
  instrument has parsed them. Reading status straight after a write can
  observe the state from before it, so poll until the expected bit appears.

Exit code 0 if every test passed, 1 otherwise.
"""

from __future__ import annotations

import argparse
import sys
import time

try:
    import pyvisa
except ImportError:
    print("ERROR: pyvisa is not installed. pip install pyvisa pyvisa-py", file=sys.stderr)
    sys.exit(2)

rm = pyvisa.ResourceManager("@py")
results: list = []

HOST = "127.0.0.1"
PAD = 23
DEAD_PAD = 5


def open_inst(pad, timeout=15000):
    inst = rm.open_resource(f"TCPIP::{HOST}::hislip{pad}::INSTR", open_timeout=timeout)
    inst.timeout = timeout
    return inst


def check(name, fn):
    t0 = time.time()
    try:
        detail = fn()
        dt = time.time() - t0
        print(f"PASS  {name}  ({dt:.2f}s)  {detail}")
        results.append((name, True))
    except Exception as e:
        dt = time.time() - t0
        text = str(e).strip() or type(e).__name__
        msg = text.splitlines()[0][:90]
        print(f"FAIL  {name}  ({dt:.2f}s)  {msg}")
        results.append((name, False))


# --- 1. identity, repeated (stability of the round trip) -------------------
def t_idn_repeat():
    inst = open_inst(PAD)
    try:
        idns = {inst.query("*IDN?").strip() for _ in range(10)}
        assert len(idns) == 1, f"inconsistent IDN across reads: {idns}"
        return f"10/10 identical -> {idns.pop()}"
    finally:
        inst.close()


# --- 2. reset + error queue ------------------------------------------------
def t_reset():
    inst = open_inst(PAD)
    try:
        inst.write("*RST")
        inst.write("*CLS")
        err = inst.query("SYST:ERR?").strip()
        assert err.startswith("+0,"), f"error queue not clean: {err}"
        return f"*RST/*CLS ok, SYST:ERR? -> {err}"
    finally:
        inst.close()


# --- 3. a real measurement -------------------------------------------------
def t_measure_dcv():
    inst = open_inst(PAD)
    try:
        v = float(inst.query("MEAS:VOLT:DC?"))
        return f"MEAS:VOLT:DC? -> {v:+.6e} V"
    finally:
        inst.close()


def t_measure_res():
    inst = open_inst(PAD)
    try:
        r = float(inst.query("MEAS:RES?"))
        return f"MEAS:RES? -> {r:.6e} ohm (open leads read ~1e37 overload)"
    finally:
        inst.close()


# --- 4. long read (>1 KB in one transfer) ----------------------------------
def t_long_read():
    inst = open_inst(PAD, timeout=30000)
    try:
        inst.write("*RST")
        inst.write("CONF:VOLT:DC 10")
        inst.write("VOLT:DC:NPLC 0.02")
        inst.write("TRIG:SOUR IMM")
        inst.write("SAMP:COUN 200")
        raw = inst.query("READ?").strip()
        vals = raw.split(",")
        assert len(vals) == 200, f"expected 200 readings, got {len(vals)}"
        [float(v) for v in vals]  # all parse
        return f"{len(raw)} bytes, {len(vals)} readings, all parsed"
    finally:
        inst.close()


# --- 5. bus-triggered acquisition (GET) ------------------------------------
def t_trigger():
    inst = open_inst(PAD)
    try:
        inst.write("*RST")
        inst.write("CONF:VOLT:DC 10")
        inst.write("VOLT:DC:NPLC 0.02")
        inst.write("SAMP:COUN 5")
        inst.write("TRIG:SOUR BUS")
        inst.write("INIT")
        inst.assert_trigger()          # real GPIB GET over HiSLIP
        vals = inst.query("FETC?").strip().split(",")
        assert len(vals) == 5, f"expected 5 readings, got {len(vals)}"
        return f"GET -> INIT/FETC? returned {len(vals)} readings"
    finally:
        inst.close()


# --- 6. serial poll / status byte ------------------------------------------
def t_status_byte():
    """A poll must report a *non-zero* status: 0 is what a broken poll returns."""
    inst = open_inst(PAD)
    try:
        inst.write("*CLS")
        inst.write("*ESE 32")            # summarise command errors into ESB
        assert inst.read_stb() == 0, "status should start clear"

        inst.write("BOGUSCMD")           # provoke a command error -> ESB (0x20)
        # A GPIB write completes when handshaken, not when parsed, so poll
        # until the bit appears rather than assuming the first read sees it.
        stb = 0
        deadline = time.time() + 2.0
        while time.time() < deadline:
            stb = inst.read_stb()
            if stb:
                break
        reported = int(inst.query("*STB?").strip())
        assert stb == 0x20, f"expected 0x20, got 0x{stb:02x}"
        assert stb == reported, f"read_stb 0x{stb:02x} != *STB? 0x{reported:02x}"

        inst.write("*CLS")
        cleared = 0xFF
        deadline = time.time() + 2.0
        while time.time() < deadline:
            cleared = inst.read_stb()
            if cleared == 0:
                break
        assert cleared == 0, f"status did not clear after *CLS: 0x{cleared:02x}"
        return f"0x00 -> 0x{stb:02x} (matches *STB?) -> 0x00"
    finally:
        inst.close()


# --- 6b. SRQ delivered as a VISA event -------------------------------------
def t_srq_event():
    """The instrument asserts SRQ; the daemon must forward it to the client."""
    inst = open_inst(PAD)
    try:
        inst.write("*CLS")
        inst.write("*ESE 32")            # command error -> ESB
        inst.write("*SRE 32")            # ESB -> SRQ
        inst.enable_event(
            pyvisa.constants.EventType.service_request,
            pyvisa.constants.EventMechanism.queue,
        )
        try:
            inst.write("BOGUSCMD")       # should raise SRQ on the bus
            # Confirm the instrument really did request service, so a failure
            # here is the transport's and not the instrument's.
            stb = int(inst.query("*STB?").strip())
            assert stb & 0x40, f"instrument did not assert RQS: 0x{stb:02x}"

            inst.wait_on_event(
                pyvisa.constants.EventType.service_request, 5000
            )
            return f"service_request event delivered (STB 0x{stb:02x})"
        finally:
            inst.disable_event(
                pyvisa.constants.EventType.service_request,
                pyvisa.constants.EventMechanism.queue,
            )
            inst.write("*SRE 0")
            inst.write("*CLS")
    finally:
        inst.close()


# --- 7. device clear -------------------------------------------------------
def t_device_clear():
    """Device clear must actually *do* something.

    Checking only that the instrument still answers afterwards passes even
    when SDC is a silent no-op — which is exactly how a mis-addressed device
    clear went unnoticed. Prove the effect instead: an initiated 34401A
    rejects a second INIT with -213, and a working clear aborts the
    measurement so the second INIT succeeds. The control arm establishes that
    the probe can fail at all.
    """
    inst = open_inst(PAD)
    try:
        def arm():
            inst.write("ABOR"); inst.write("*CLS"); inst.write("*RST")
            time.sleep(0.5)
            inst.write("CONF:VOLT:DC 10")
            inst.write("TRIG:SOUR BUS")
            inst.write("INIT")
            time.sleep(0.3)

        def second_init():
            inst.write("INIT")
            time.sleep(0.3)
            return inst.query("SYST:ERR?").strip()

        arm()
        control = second_init()
        assert control.startswith("-213"), (
            f"probe invalid: initiated instrument accepted a second INIT ({control})"
        )

        arm()
        inst.clear()
        time.sleep(0.3)
        after = second_init()
        assert after.startswith("+0"), f"device clear did not abort the measurement ({after})"

        inst.write("ABOR"); inst.write("*CLS"); inst.write("*RST")
        return "aborts an initiated measurement (control -213, after clear +0)"
    finally:
        inst.close()


# --- 8. RESILIENCE: dead address must not wedge the bus --------------------
def t_dead_address_isolated():
    """A timeout against an empty address must not break the next good query."""
    dead = open_inst(DEAD_PAD, timeout=4000)
    try:
        try:
            dead.query("*IDN?")
            raise AssertionError(f"PAD {DEAD_PAD} answered but should be empty")
        except pyvisa.errors.VisaIOError:
            pass  # expected timeout
    finally:
        dead.close()

    inst = open_inst(PAD)
    try:
        idn = inst.query("*IDN?").strip()
        assert "34401A" in idn, f"bus wedged after dead-address timeout: {idn!r}"
        return f"timeout at PAD {DEAD_PAD}, PAD {PAD} still fine"
    finally:
        inst.close()


def t_repeated_dead_then_live():
    """Several consecutive dead-address timeouts, then a live query."""
    for _ in range(3):
        d = open_inst(DEAD_PAD, timeout=4000)
        try:
            try:
                d.query("*IDN?")
            except pyvisa.errors.VisaIOError:
                pass
        finally:
            d.close()
    inst = open_inst(PAD)
    try:
        idn = inst.query("*IDN?").strip()
        assert "34401A" in idn, f"bus wedged after 3 timeouts: {idn!r}"
        return "3 consecutive timeouts, PAD 23 still fine"
    finally:
        inst.close()


# --- 9. concurrent sessions ------------------------------------------------
def t_two_sessions():
    a = open_inst(PAD)
    b = open_inst(PAD)
    try:
        ia = a.query("*IDN?").strip()
        ib = b.query("*IDN?").strip()
        assert ia == ib, f"{ia!r} != {ib!r}"
        return "two concurrent HiSLIP sessions agree"
    finally:
        a.close()
        b.close()


TESTS = [
    ("idn x10", t_idn_repeat),
    ("reset + error queue", t_reset),
    ("measure DCV", t_measure_dcv),
    ("measure resistance", t_measure_res),
    ("long read (200 rdgs)", t_long_read),
    ("bus trigger (GET)", t_trigger),
    ("serial poll (STB)", t_status_byte),
    ("SRQ event", t_srq_event),
    ("device clear", t_device_clear),
    ("dead addr isolated", t_dead_address_isolated),
    ("3x dead then live", t_repeated_dead_then_live),
    ("two sessions", t_two_sessions),
]


def main() -> int:
    global HOST, PAD, DEAD_PAD
    ap = argparse.ArgumentParser(description="ugpibd hardware exercise")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--pad", type=int, required=True, help="instrument GPIB primary address")
    ap.add_argument(
        "--dead-pad",
        type=int,
        default=5,
        help="an address with nothing attached, for the timeout tests",
    )
    args = ap.parse_args()
    if args.pad == args.dead_pad:
        ap.error("--dead-pad must differ from --pad")
    HOST, PAD, DEAD_PAD = args.host, args.pad, args.dead_pad

    for name, fn in TESTS:
        check(name, fn)

    print()
    ok = sum(1 for _, passed in results if passed)
    print(f"{ok}/{len(results)} passed")
    return 0 if ok == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
