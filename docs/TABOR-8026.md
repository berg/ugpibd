# Tabor 8026: lost first bytes, and how to stop them

A Tabor Electronics 8026 on an **NI GPIB-USB-HS** silently loses the first byte
of a response whenever the read is issued some time after the query. Short
responses are lost **entirely** — a one-character answer comes back empty, with
no error anywhere.

**On an Agilent 82357A the instrument is clean with no action at all**: 560
reads across every response length, both failure routes, and delays out to
3 seconds, zero losses, nothing configured. If you have the choice, use that
adapter with this instrument and stop reading here.

On an NI adapter it is fixed by configuring the instrument, with two command
bytes:

```
cargo run --example hs488_config -- 127.0.0.1 9010 4
```

Re-run it after every power cycle of the instrument. Nothing else is needed:
no change to the daemon, no special read path, no timing settings.

## What it looks like when it bites

Measured on a GPIB-USB-HS with the instrument at pad 4, 100 delayed reads per
query:

| query | response | unconfigured | after `hs488_config` |
|-------|----------|--------------|----------------------|
| `*OPC?` | `"1"` | **100/100 lost** | 0/100 |
| `*SRE?` | `"0"` | **100/100 lost** | 0/100 |
| `*IDN?` | 31 chars | 11/100 truncated | 0/100 |

An HP 34401A on the same bus is unaffected at every response length, before and
after.

Two things hide it. The HiSLIP front end reads a response as soon as the write
completes, which never gives the instrument time to get into the failing state,
so the bug mostly appears on the VXI-11 path with a client-side delay. And a
truncated 31-character string still looks like a plausible answer, while a
missing leading minus sign or digit does not announce itself at all.

## Adapter dependence

| adapter | unconfigured 8026 | after `hs488_config` |
|---------|-------------------|----------------------|
| NI GPIB-USB-HS | 100% loss on 1-char responses, 11% on 31-char | clean |
| Agilent 82357A | **clean** | not needed |

Same instrument, same bus, same daemon, same addressing sequence. The
difference is the controller chip and its firmware: the 82357's TMS9914
acceptor free-runs, so the byte the 8026 sources at the ATN edge is accepted
and *delivered*. The NI adapter's TNT4882 accepts it too, and then its read
operation resets the FIFO and throws it away.

That is worth stating plainly: the data loss requires the NI adapter's
firmware. The instrument's early sourcing is out of spec, but on its own it
costs nothing.

## Why it happens

Three facts, each measured on the bus with a logic analyser:

1. When the 8026 has been addressed to talk long enough to prepare its answer,
   it sources the first byte **within 100 ns of ATN being released**, with no
   data settling time. IEEE 488.1 Table 39 requires at least 1100 ns for the
   first byte after each false transition of ATN. The instrument's own firmware
   asks its chip for the longest settling time available (2000 ns), so this is
   the part not behaving as its manual documents, and it remains unexplained.
2. At that instant the controller's TNT4882 is briefly ready — `rdy` is forced
   true while ATN is asserted — but the adapter firmware has not armed a read
   operation yet. It accepts the byte and completes the handshake.
3. The read operation, when it does arrive one USB round trip later, begins by
   resetting the chip's FIFO. The accepted byte is discarded. The response
   arrives one byte short, carrying EOI and a matching byte count, so nothing
   downstream can tell.

Neither side alone loses data: an instrument that waits, as both HP instruments
here do, is read correctly, and a controller with a read armed would keep the
byte. It takes both.

## Why the fix works

`CFE` (0x1F) followed by `CFG15` (0x6F), sent under ATN to the instrument
addressed as a listener, enables HS488 in its TNT4882. The documented HS488
start-up has the *talker* assert NRFD itself for a few hundred nanoseconds
after ATN falls, before sourcing the first byte. That removes the instant in
which an unarmed controller can accept anything.

Afterwards the first byte arrives 322-402 us after the ATN edge, which is the
controller's read-arm latency — the instrument is now waiting for RFD, as a
conforming source handshake should. The cure therefore does not depend on
controller timing at all.

## Caveats

* **It lasts until the instrument is power cycled**, and nothing reports it:
  the relevant register is write-only and the firmware never reads it back. The
  tool verifies by behaviour instead, issuing 20 delayed `*OPC?` reads and
  requiring `"1"` every time.
* **Send it only when the instrument is idle.** Both bytes are command
  pass-through, and the instrument answers them from an interrupt handler that
  does not run while a transfer is in flight. A command byte sent then hangs
  the bus until the instrument is power cycled. The tool drains first.
* NI documents HS488 as requiring the chip to be clocked at 40 MHz. This
  instrument's clock has not been measured.
* Verified against SCPI queries from 1 to 31 characters, serial polls, chunked
  reads and the full `bench_regression` matrix. **Not** verified against large
  binary waveform downloads, which is what this instrument is mainly for.

## What not to do instead

Shrinking the controller's addressing-to-ATN gap does reduce the loss, and it
is tempting because it needs no instrument-side action. It is not a fix. A
sweep of that gap, holding everything else constant:

| gap | `*OPC?` lost | `*IDN?` lost |
|-----|--------------|--------------|
| 231 us | 23% | 3% |
| +200 us | 55% | 8% |
| +300 us | 98% | 8% |
| +400 us | 100% | 48% |

It is a pure race, so any mitigation of this kind changes the odds for every
instrument on the bus in order to suit one, and leaves the data loss silent
when it does occur.

## Tools

* `examples/hs488_config.rs` — applies the fix and verifies it.
## Doing it from your own client

Nothing here is required: the payload is four command bytes, `3F 24 1F 6F`
(UNL, MLA*pad*, CFE, CFG15), sent through VXI-11 Send Command — `device_docmd`
with cmd `0x020000` on an interface link (`gpib0`). Any client that can reach
that operation can apply the fix itself.

Two notes for Python callers. pyvisa exposes `send_command` only for local GPIB
boards, not over TCPIP, so it has to go through pyvisa-py's VXI-11 layer
directly, which does implement the operation. And `vxi11.CoreClient` looks the
port up through a portmapper on 111, which is not running unless
`ugpibd-portmap` is; connect to the fixed VXI-11 port instead.
* `examples/length_sweep.rs` — loss rate against response length.
* `examples/poll_then_read.rs` — loss rate for reads that follow a serial poll,
  with a no-poll control arm.

The measurement harnesses pin their expected answers in source rather than
probing the instrument for them. At a 100% loss rate a probed reference defines
the truncated answer as correct and reports a clean run on a completely broken
path; that happened twice while this was being characterised.
