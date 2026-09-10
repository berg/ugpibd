#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Exercise a running ugpibd against one USBTMC/USB488 instrument over VXI-11.

The USBTMC counterpart to hardware_exercise.py (which is written for a GPIB
DMM and will time out — and can wedge — against a scope or generator). This
one uses only IEEE 488.2 common commands, so it runs against any USB488
instrument with the standard status model: a Siglent SDG, a Rigol scope, and
most bench gear.

    uv run --with pyvisa --with pyvisa-py contrib/usbtmc_exercise.py

By default it talks to the VXI-11 core on 127.0.0.1:9010, address 0 (the
USBTMC backend ignores the address — one interface is one instrument). Point
it elsewhere with --host/--port, or hand it a whole resource string with -r.

What it checks, and why each is here rather than assumed from *IDN? working:

* Serial poll returns a byte, and it equals *STB?. On a USB488 device with an
  interrupt endpoint (the common case) this exercises the interrupt-endpoint
  status path, which is the one that needed a fallback on a Rigol.
* A command error raises the ESB summary bit in the status byte — a serial
  poll that can actually change, not one stuck at 0.
* A service request is *pushed*: enable SRQ on that same command error and
  wait for the event, rather than polling for it. Needs a pyvisa-py whose
  VXI-11 backend implements the interrupt channel; reported as skipped, not
  failed, where it does not.
* Trigger (the USB488 TRIGGER message) is accepted.
* Remote then local (REN_CONTROL / GO_TO_LOCAL) are accepted; watch the front
  panel to confirm the effect.
* Timeout recovery: an unsupported query fails, and the very next *IDN? still
  answers. This is the one that used to wedge a Rigol, so it runs last and
  loudly.

Capability gaps are reported as SKIP, not FAIL: a plain USBTMC interface (no
USB488) legitimately refuses serial poll, trigger and remote/local, and the
daemon says so. A genuine misbehaviour — a poll that never changes, a
recovery that leaves the instrument mute — is a FAIL.

Exit code 0 if nothing FAILed, 1 otherwise.
"""

from __future__ import annotations

import argparse
import sys
import time

try:
    import pyvisa
    from pyvisa import constants
except ImportError:
    sys.exit("pyvisa not found; run with: uv run --with pyvisa --with pyvisa-py " + __file__)

# A command no instrument implements, to provoke a command error and to make a
# read time out. Kept obviously bogus so it cannot accidentally do something.
BOGUS = "UGPIBD:NO:SUCH:COMMAND?"

PASS, FAIL, SKIP = "PASS", "FAIL", "SKIP"


class Report:
    def __init__(self) -> None:
        self.failed = 0

    def line(self, status: str, name: str, detail: str = "") -> None:
        tail = f"  {detail}" if detail else ""
        print(f"{status:4}  {name}{tail}")
        if status == FAIL:
            self.failed += 1


def poll_stb(inst) -> int:
    """read_stb, normalised to an int."""
    return int(inst.read_stb())


def wait_for_bit(inst, mask: int, timeout_s: float = 2.0) -> int:
    """Serial-poll until `mask` appears in the status byte, or time out.

    A write completes when its bytes are accepted, not when the instrument
    has acted on them, so the bit may lag the command that sets it.
    """
    deadline = time.monotonic() + timeout_s
    stb = poll_stb(inst)
    while stb & mask == 0 and time.monotonic() < deadline:
        time.sleep(0.02)
        stb = poll_stb(inst)
    return stb


def main() -> int:
    ap = argparse.ArgumentParser(description="Exercise ugpibd's USBTMC backend over VXI-11")
    ap.add_argument("-r", "--resource", help="full VISA resource string (overrides host/port/addr)")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=9010, help="VXI-11 core port (default 9010)")
    ap.add_argument("--addr", type=int, default=0, help="GPIB address in the resource (ignored by the backend)")
    ap.add_argument("--timeout-ms", type=int, default=4000)
    args = ap.parse_args()

    resource = args.resource or f"TCPIP::{args.host},{args.port}::gpib0,{args.addr}::INSTR"
    rep = Report()

    rm = pyvisa.ResourceManager("@py")
    print(f"opening {resource}")
    inst = rm.open_resource(resource)
    inst.timeout = args.timeout_ms

    # --- identity ------------------------------------------------------------
    try:
        idn = inst.query("*IDN?").strip()
        rep.line(PASS, "identity", idn)
    except Exception as e:  # noqa: BLE001
        rep.line(FAIL, "identity", repr(e))
        print("\ncannot identify the instrument; stopping.")
        return 1

    # --- serial poll matches *STB? ------------------------------------------
    # Clear first so the byte is in a known state, then compare the two ways of
    # reading it. They must agree: read_stb is the USB488 READ_STATUS_BYTE,
    # *STB? is the instrument computing the same byte itself.
    supports_status = True
    try:
        inst.write("*CLS")
        stb = poll_stb(inst)
        star = int(inst.query("*STB?").strip())
        if stb == star:
            rep.line(PASS, "serial poll == *STB?", f"0x{stb:02x}")
        else:
            rep.line(FAIL, "serial poll == *STB?", f"read_stb 0x{stb:02x} != *STB? 0x{star:02x}")
    except pyvisa.errors.VisaIOError as e:
        if e.error_code == constants.StatusCode.error_nonsupported_operation:
            supports_status = False
            rep.line(SKIP, "serial poll == *STB?", "read_stb not supported by this pyvisa-py")
        else:
            rep.line(FAIL, "serial poll == *STB?", repr(e))
    except Exception as e:  # noqa: BLE001
        # The daemon refuses serial poll on a plain (non-488) USBTMC interface.
        rep.line(SKIP, "serial poll == *STB?", str(e).splitlines()[0])
        supports_status = False

    # --- a command error raises the ESB summary bit --------------------------
    # *ESE 255 routes every event-status bit into the ESB summary (STB bit 5);
    # a bogus command sets the command-error bit. So the status byte must go
    # from clear to 0x20-set. This is the control that proves the poll above
    # can actually change, not just report a constant 0.
    esb_ok = False
    if supports_status:
        try:
            inst.write("*CLS")
            inst.write("*ESE 255")
            base = poll_stb(inst)
            inst.write(BOGUS.rstrip("?"))  # a bogus *write*, no reply expected
            stb = wait_for_bit(inst, 0x20)
            if base & 0x20 == 0 and stb & 0x20:
                rep.line(PASS, "command error -> ESB", f"0x{base:02x} -> 0x{stb:02x}")
                esb_ok = True
            else:
                rep.line(FAIL, "command error -> ESB", f"0x{base:02x} -> 0x{stb:02x} (bit 5 unchanged)")
        except Exception as e:  # noqa: BLE001
            rep.line(FAIL, "command error -> ESB", repr(e))

    # --- service request is pushed, not polled -------------------------------
    # Enable SRQ on the same ESB condition and wait for the event. This is the
    # interrupt-endpoint push path end to end (device -> interrupt IN -> VXI-11
    # interrupt channel -> client). Skipped, not failed, if this pyvisa-py has
    # no VXI-11 event support.
    if supports_status and esb_ok:
        try:
            inst.write("*CLS")
            inst.write("*ESE 255")
            inst.write("*SRE 0x20")  # ESB summary -> SRQ
            inst.enable_event(constants.EventType.service_request, constants.EventMechanism.queue)
            inst.write(BOGUS.rstrip("?"))
            try:
                inst.wait_on_event(constants.EventType.service_request, args.timeout_ms)
                stb = poll_stb(inst)
                if stb & 0x40:
                    rep.line(PASS, "service request pushed", f"RQS set, stb 0x{stb:02x}")
                else:
                    rep.line(FAIL, "service request pushed", f"event fired but RQS clear (0x{stb:02x})")
            finally:
                inst.disable_event(constants.EventType.service_request, constants.EventMechanism.queue)
                inst.write("*SRE 0")
                inst.write("*CLS")
        except (pyvisa.errors.VisaIOError, NotImplementedError, AttributeError) as e:
            code = getattr(e, "error_code", None)
            if code == constants.StatusCode.error_timeout:
                rep.line(FAIL, "service request pushed", "no SRQ event within the timeout")
            else:
                rep.line(SKIP, "service request pushed", "this pyvisa-py has no VXI-11 SRQ events")
        except Exception as e:  # noqa: BLE001
            rep.line(SKIP, "service request pushed", str(e).splitlines()[0])

    # --- trigger (USB488 TRIGGER message) ------------------------------------
    try:
        inst.assert_trigger()
        rep.line(PASS, "trigger accepted", "USB488 TRIGGER message sent")
    except pyvisa.errors.VisaIOError as e:
        if e.error_code == constants.StatusCode.error_nonsupported_operation:
            rep.line(SKIP, "trigger accepted", "assert_trigger not supported by this pyvisa-py")
        else:
            # The daemon refuses trigger when the device does not advertise it.
            rep.line(SKIP, "trigger accepted", repr(e))
    except Exception as e:  # noqa: BLE001
        rep.line(SKIP, "trigger accepted", str(e).splitlines()[0])

    # --- remote then local ---------------------------------------------------
    # No reliable readback, so this only confirms the requests are accepted;
    # watch the front panel for the remote indicator to come on and go off.
    try:
        inst.control_ren(constants.RENLineOperation.asrt)  # remote
        time.sleep(0.3)
        inst.control_ren(constants.RENLineOperation.asrt_llo)  # remote + local lockout
        time.sleep(0.3)
        inst.control_ren(constants.RENLineOperation.deassert_gtl)  # go to local
        rep.line(PASS, "remote/local accepted", "watch the front panel: remote came on then off")
    except pyvisa.errors.VisaIOError as e:
        if e.error_code == constants.StatusCode.error_nonsupported_operation:
            rep.line(SKIP, "remote/local accepted", "control_ren not supported by this pyvisa-py")
        else:
            rep.line(SKIP, "remote/local accepted", repr(e))
    except Exception as e:  # noqa: BLE001
        rep.line(SKIP, "remote/local accepted", str(e).splitlines()[0])

    # --- timeout recovery ----------------------------------------------------
    # The one that used to wedge a Rigol. An unsupported query must time out,
    # and the instrument must answer the very next command. Run last so a
    # wedge here does not hide earlier results.
    try:
        try:
            inst.query(BOGUS)
            rep.line(FAIL, "recovery after timeout", "the bogus query unexpectedly answered")
        except pyvisa.errors.VisaIOError as e:
            if e.error_code != constants.StatusCode.error_timeout:
                rep.line(FAIL, "recovery after timeout", f"expected a timeout, got {e!r}")
                raise RuntimeError from e
            idn2 = inst.query("*IDN?").strip()
            if idn2 == idn:
                rep.line(PASS, "recovery after timeout", "identity answered again after the timeout")
            else:
                rep.line(FAIL, "recovery after timeout", f"identity changed: {idn2!r}")
    except Exception as e:  # noqa: BLE001
        rep.line(FAIL, "recovery after timeout", repr(e))

    inst.close()
    print()
    if rep.failed:
        print(f"{rep.failed} test(s) FAILED")
        return 1
    print("no failures")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
