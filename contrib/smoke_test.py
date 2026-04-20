#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Smoke test for a running gpibd: exercises HiSLIP and Prologix front-ends
against the same instrument and reports any behavioral mismatch.

Usage:
    ./smoke_test.py --pad 14                          # both, localhost
    ./smoke_test.py --host 192.168.1.5 --pad 15
    ./smoke_test.py --pad 14 --only hislip
    ./smoke_test.py --pad 14 --query "MEAS? 0"        # SR620-style query

Requires `pyvisa` and `pyvisa-py`:

    pip install pyvisa pyvisa-py

Exit codes: 0 on success (both front-ends returned a non-empty IDN that
matched), 1 on mismatch, 2 on connection/transport failure.
"""

from __future__ import annotations

import argparse
import sys
import time
from dataclasses import dataclass
from typing import Optional

try:
    import pyvisa
except ImportError:
    print("ERROR: pyvisa is not installed. pip install pyvisa pyvisa-py", file=sys.stderr)
    sys.exit(2)


@dataclass
class Result:
    idn: Optional[str]
    query_response: Optional[str]
    error: Optional[str] = None


def _trim(s: str) -> str:
    # Strip both the SCPI device's termination (often CR+LF) and any
    # whitespace pyvisa might have left behind.
    return s.rstrip("\r\n\t ").lstrip()


def test_hislip(host: str, port: int, pad: int, query: Optional[str], timeout_ms: int) -> Result:
    rm = pyvisa.ResourceManager("@py")
    resource = f"TCPIP::{host}::hislip0,{pad}::INSTR"
    if port != 4880:
        # pyvisa-py doesn't accept a non-default port in the resource
        # string; it always connects to 4880. Warn if the user asked for
        # something else.
        print(f"WARNING: HiSLIP port {port} requested but pyvisa-py will use 4880", file=sys.stderr)
    print(f"[hislip]   open {resource}")
    try:
        inst = rm.open_resource(resource, open_timeout=timeout_ms)
        inst.timeout = timeout_ms
        idn = _trim(inst.query("*IDN?"))
        print(f"[hislip]   *IDN? -> {idn!r}")
        qr = None
        if query:
            qr = _trim(inst.query(query))
            print(f"[hislip]   {query!r} -> {qr!r}")
        inst.close()
        return Result(idn=idn, query_response=qr)
    except Exception as e:
        return Result(idn=None, query_response=None, error=str(e))


def test_prologix(host: str, port: int, pad: int, query: Optional[str], timeout_ms: int) -> Result:
    rm = pyvisa.ResourceManager("@py")
    resource = f"TCPIP::{host}::{port}::SOCKET"
    print(f"[prologix] open {resource}")
    try:
        inst = rm.open_resource(
            resource,
            open_timeout=timeout_ms,
            read_termination="\n",
            write_termination="\n",
        )
        inst.timeout = timeout_ms
        inst.write("++mode 1")
        inst.write(f"++addr {pad}")
        inst.write("++auto 1")
        inst.write("++eoi 1")
        inst.write("++read_tmo_ms 3000")
        # Let the device process the config.
        time.sleep(0.05)
        idn = _trim(inst.query("*IDN?"))
        print(f"[prologix] *IDN? -> {idn!r}")
        qr = None
        if query:
            qr = _trim(inst.query(query))
            print(f"[prologix] {query!r} -> {qr!r}")
        inst.close()
        return Result(idn=idn, query_response=qr)
    except Exception as e:
        return Result(idn=None, query_response=None, error=str(e))


def main() -> int:
    ap = argparse.ArgumentParser(description="gpibd smoke test")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--pad", type=int, required=True, help="GPIB primary address (0-30)")
    ap.add_argument("--prologix-port", type=int, default=1234)
    ap.add_argument("--hislip-port", type=int, default=4880)
    ap.add_argument("--only", choices=["hislip", "prologix"], help="Run only one front-end")
    ap.add_argument("--query", help="Additional SCPI query to run after *IDN?")
    ap.add_argument("--timeout-ms", type=int, default=5000)
    args = ap.parse_args()

    run_hislip = args.only != "prologix"
    run_prologix = args.only != "hislip"

    hislip: Optional[Result] = None
    prologix: Optional[Result] = None

    if run_hislip:
        hislip = test_hislip(args.host, args.hislip_port, args.pad, args.query, args.timeout_ms)
        if hislip.error:
            print(f"[hislip]   FAIL: {hislip.error}", file=sys.stderr)

    if run_prologix:
        prologix = test_prologix(
            args.host, args.prologix_port, args.pad, args.query, args.timeout_ms
        )
        if prologix.error:
            print(f"[prologix] FAIL: {prologix.error}", file=sys.stderr)

    # Evaluate.
    failures = []
    if hislip and hislip.error:
        failures.append("hislip transport error")
    if prologix and prologix.error:
        failures.append("prologix transport error")
    if hislip and prologix and not hislip.error and not prologix.error:
        if hislip.idn != prologix.idn:
            failures.append(
                f"IDN mismatch: hislip={hislip.idn!r} prologix={prologix.idn!r}"
            )
        if args.query and hislip.query_response != prologix.query_response:
            failures.append(
                "query mismatch: "
                f"hislip={hislip.query_response!r} prologix={prologix.query_response!r}"
            )
    if hislip and not hislip.error and not hislip.idn:
        failures.append("hislip returned empty IDN")
    if prologix and not prologix.error and not prologix.idn:
        failures.append("prologix returned empty IDN")

    if failures:
        print("\nFAIL", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1 if not any("transport error" in f for f in failures) else 2

    print("\nOK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
