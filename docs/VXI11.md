# VXI-11 front-end

The VXI-11 (TCP/IP Instrument Protocol) server listens on port 9010 by
default (`--vxi11-port`, 0 disables). VXI-11 has no IANA-assigned port; the
spec expects portmapper discovery, which the optional `ugpibd-portmap`
binary provides — see "Discovery" below.

VXI-11 is the front-end whose wire protocol matches GPIB semantics:
`device_read` is an explicit RPC, so the daemon addresses the instrument to
talk because the client asked it to. The read-after-write heuristic that
HiSLIP forces on a GPIB bridge (see HISLIP.md) has no counterpart here.
Instruments that only produce output once addressed — screen dumps, plot
transfers, anything pre-488.2 — work over VXI-11 and cannot work over
HiSLIP.

## Resource strings

pyvisa-py carries the port in the host token:

    TCPIP::<host>,9010::gpib0,<pad>::INSTR     a device at <pad>
    TCPIP::<host>,9010::inst0::INSTR           the daemon's default PAD
    TCPIP::<host>,9010::gpib0::INSTR           the interface itself

With the portmapper running, the port can be omitted entirely
(`TCPIP::<host>::gpib0,<pad>::INSTR`), which is also the form NI and
Keysight VISA produce.

Device-name parsing follows VXI-11.2 §B.1: `gpib0,<pad>[,<sad>]`, bare
`gpib0` for the interface link, plus `inst0` for the daemon default PAD
(our extension, matching the HiSLIP sub-address convention). Errors come
from the spec's own table: secondary addresses parse but are refused with
21 (no backend can address one yet — ROADMAP), `gpibN` for N≠0 is 3,
garbage is 1.

## Timeouts

Every RPC carries its own `io_timeout`, honored per operation; 0 means the
daemon's `--timeout-ms`. Internally the deadline is enforced daemon-side —
reads poll the bus in short slices, writes go out in chunks whose adapter
timeout is the remaining budget floored to an exact adapter step — because
adapter timeout hardware is a coarse code table that rounds *up*, while a
VXI-11 client only grants the server `io_timeout` plus a small grace before
declaring the connection dead. A timed-out read returns error 15 with the
partial data and reason 0 (RULE B.6.27); a timed-out write reports the
accepted byte count (RULE B.6.21).

## Locking

`device_lock`/`device_unlock` and `create_link`'s `lockDevice` are enforced
against the same daemon-wide registry as HiSLIP's `viLock`: a lock taken
over either protocol excludes I/O arriving over the other, because it is
the same instrument. VXI-11 semantics per the spec: exclusive, held by the
*link* (two links on one connection contend like strangers), non-nesting —
a re-lock by the holder is error 11 (RULE B.6.72) — and released on unlock,
on `destroy_link`, and when the core connection breaks (RULE B.6.77).
`waitlock` bounds the wait on every I/O operation.

## Abort

The abort channel (DEVICE_ASYNC) runs on its own ephemeral port, reported
in every `Create_LinkResp`. `device_abort` terminates the link's in-flight
call at its nearest safe point: between read slices or write chunks (a bus
transaction underway always completes; partial data / accepted counts still
go back), immediately for a lock wait. `destroy_link`, `device_enable_srq`
and `device_unlock` are exempt per RULE B.6.106; an abort with nothing in
flight is a delivered no-op (OBSERVATION B.6.24).

## Service requests

`create_intr_chan` connects back to the client's DEVICE_INTR server over
TCP (UDP is refused with 8, a permitted refusal); `device_enable_srq`
registers a per-link handle, echoed byte-for-byte (RULE B.6.111). Enable
state survives channel destruction (OBSERVATION B.6.21), and enabling while
the SRQ line is already high notifies immediately (VXI-11.2 RULE B.4.14).

**Deliberate divergence:** VXI-11.2 RULE B.4.13 notifies *every* enabled
link on any SRQ edge. This daemon serial-polls instead and notifies only
the links whose instrument has RQS set — one poll per instrument — so a
device link is never woken for another instrument's request. Interface
links, which have no instrument to poll, get the spec's line-level
behavior. The poll consumes RQS at the instrument; the single-consumer
caveat documented in HISLIP.md therefore spans front-ends: whichever
forwarder polls first takes the byte.

## The interface link and device_docmd

A link to bare `gpib0` is the interface itself (VXI-11.2 RULE B.1.3). It
serves the §B.5 docmd set — Send Command (raw bytes under ATN), Bus Status
(all eight Table B.2 selectors, the line states read live), ATN Control,
REN Control, Bus Address (really re-addresses the controller), IFC — with
Table B.1's size grid enforced (error 5) and values in the client's
declared byte order. Interface-link `device_clear` is bus-wide DCL,
`device_trigger` is unaddressed GET, and `device_write`/`device_read` move
data with *no addressing sequence* (SEND DATA BYTES / RECEIVE RESPONSE
MESSAGE), which is the legacy-instrument escape of RECOMMENDATION B.1.1:
address by hand with Send Command, then transfer.

`device_docmd` on a *device* link answers 8 with no action (RULE B.5.2).

### Deviations

- **Pass Control (docmd 0x020004) is refused with error 8, permanently**
  (signed off 2026-08-10). This is an architectural decision, not a gap:
  the daemon is the bus's only controller — every front-end, lock, and SRQ
  forwarder assumes it — and passing control to another device would
  strand them all. A conforming client sees a clean refusal on a command
  that only matters in multi-controller systems, which this daemon does
  not participate in.
- ~~ATN Control on the 82357~~ — verified on hardware (82357A): the
  kernel-driver take-control transcription asserts and releases cleanly.

## Discovery

`ugpibd-portmap` (separate binary, optional deb) provides portmapper
presence for clients that discover VXI-11 via port 111. It picks its own
mode at startup: with rpcbind present (Debian's default) it registers
program 0x0607AF with it — cooperative, self-healing across rpcbind
restarts, withdrawn on shutdown; with no portmapper present it serves the
port itself. `systemctl enable --now ugpibd-portmap` is the entire setup.

## What clients work

- **pyvisa-py** (stock): everything — resource strings above, per-call
  timeouts, locks, and with the maintained fork, SRQ events.
- **NI / Keysight VISA**: discovery via the portmapper, then the standard
  VXI-11 they were built on (this protocol is what their LAN/GPIB
  gateways, E2050/E5810, speak natively).
- **`ugpibd-scpi --transport vxi11`**: the bundled REPL, including
  `++read` for talks-when-addressed instruments.
