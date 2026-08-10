# Changelog

Notable changes per release. The entry for a version is what GitHub publishes
as its release notes, so it is written for someone deciding whether to upgrade
rather than for someone reading the diff — `scripts/release` refuses to tag a
version that has no entry here.

## v0.7.0 — 2026-08-10

A full VXI-11 front-end beside HiSLIP — and with it, the class of
instrument no HiSLIP bridge can serve: ones that only produce output once
addressed to talk. An 859x screen dump or a plotter transfer now works
with stock pyvisa-py and unmodified tools (`visashot -s
"TCPIP::host,9010::gpib0,18::INSTR"`), because VXI-11's `device_read` puts
the client's read on the wire instead of leaving the daemon to guess.

### VXI-11 (port 9010, on by default)

Everything the spec defines, tested against it and against four adapters
on real instruments:

- per-call `io_timeout`s, enforced daemon-side so replies always beat the
  client's own deadline regardless of adapter timeout granularity
- locking that is shared with HiSLIP — a `viLock` over either protocol
  excludes I/O over the other — with VXI-11's own per-link, non-nesting
  semantics
- a real abort channel: `device_abort` interrupts an in-flight call at the
  nearest safe point, never mid-bus-transaction
- SRQ events delivered over the interrupt channel, per-instrument (a link
  is only woken for its own instrument's request)
- the VXI-11.2 interface device: a bare `gpib0` link with the full docmd
  set — raw command bytes, live bus status, ATN/REN control, bus-wide
  clear, unaddressed data transfer for legacy addressing sequences

Resource strings and conformance notes, including the two documented
deviations, are in `docs/VXI11.md`.

### Discovery for NI and Keysight VISA

The optional `ugpibd-portmap` package answers portmapper lookups so
`TCPIP::host::gpib0,15::INSTR` works with no port in the string. One
command (`systemctl enable --now ugpibd-portmap`), and it picks its own
mode: register with system rpcbind where one runs (Debian and Raspberry
Pi OS default), serve port 111 itself where none does.

### The CLI speaks all three front-ends

`ugpibd-scpi --transport hislip|vxi11|prologix`, plus a new `++read`
meta-command — the explicit addressed read, for the same
talks-when-addressed instruments — and a `++help` that actually explains
things.

### Fixed

- The 82357 backends no longer have the bus reset out from under a slow
  query: the read path now respects each adapter's timeout granularity
  instead of polling the 82357's heavyweight timeout path.
- CI publishes plain binary tarballs (linux amd64/arm64, macOS arm64)
  alongside the debs.

## v0.6.0 — 2026-08-06

Instruments that plot or print to GPIB can now be captured, on every adapter.
The daemon starts on an empty bus. And whether a HiSLIP command gets a read
after its write is now decided by the instrument, not by grepping the command
for a question mark.

### Capture plots and prints off the bus

An instrument that plots or prints never answers a query: it drives the bus as
a talker, and until a listener exists it emits nothing at all. Two new modes
cover the two ways instruments do this — which one you need is a property of
the instrument:

- `++lon` / `--listen-only` — unaddressed listen, for a talk-only source
- `++dev` / `--listen-address` — act as a GPIB device at a primary address,
  for instruments that address their plotter

Both stream raw bytes to `--capture-port`, unframed and uninterpreted; writes
are refused while a mode is active, and leaving one re-initialises the
controller. `++lines` reports the eight bus control lines — the first thing to
reach for when a capture is silent, because it distinguishes an instrument
that is not transmitting from one transmitting where we cannot hear it.

Verified on real hardware on both adapter families: an HP 53310A prints a full
page of PCL raster and an SRS SR620 plots HP-GL through device mode on the NI
GPIB-USB-HS+, and the same 53310A print captures byte-identical through an
82357B.

Getting the 82357 there fixed real faults: without RFD holdoff the TMS9914
acknowledged every byte on its own and the data was gone before the daemon
could collect it (the talker finishes convinced it had a listener — it did),
a timed-out read discarded everything it had already collected, and a read the
adapter refused for not being a listener was indistinguishable from a quiet
bus. Along the way, two general 82357 faults that produced plausible wrong
answers rather than errors: a USB transfer cancelled mid-flight desynchronised
the next transaction, and init did not drain a pipe left dirty by a dead
predecessor.

### The daemon starts on an empty bus

Starting the daemon with the adapter plugged in but no instrument powered on
failed as fatal — "no devices on the GPIB bus" — because init's trailing
self-addressing needs an acceptor and there was none. Plugging in the adapter
before the instruments is the obvious order to try, which made this a
first-use blocker. The check is now non-fatal and quick: the daemon starts,
serves clients, reports per-operation errors while the bus is empty, and an
instrument powered on later answers its first query.

### The instrument decides whether a write gets a read

HiSLIP read-after-write was decided by sniffing the command text for `?`, with
both failure directions seen at the bench: `DISP:TEXT "why?"` timed out and
left `-410` in the instrument's error queue, and output the sniff missed was
stranded to corrupt the next exchange. The text is now only a quote-aware
hint — a `?` inside a string literal no longer looks like a query — and when
the hint says write-only, the daemon serial-polls once after the write and
reads anyway if MAV is set. The `ugpibd-scpi` client shared the same sniff
(a false positive froze the REPL) and now shares the fix.

### Docs

The README is cut down to intro, adapters, install, and usage. Protocol detail
moved to `docs/` — HiSLIP locking and SRQ semantics in `docs/HISLIP.md`, the
`++` command reference in `docs/PROLOGIX.md`, capture internals in
`docs/CAPTURE.md`.

### Behaviour changes worth knowing before upgrading

- Write-only HiSLIP commands carry the cost of one serial poll after the
  write. Genuine queries keep the exact read path they had.
- An empty bus at startup is no longer an error; operations report "no
  devices" individually instead.
- While a capture mode is active, writes are refused.

## v0.5.0 — 2026-08-02

The HiSLIP front-end went from "works for one client at a time" to conformant.
A protocol checker driven against the daemon went from 16 of 25 checks passing
to 28 of 28, and a fix in here made ordinary query throughput about twenty times
faster on instruments that had been left in local.

### Locking is real

`viLock` / `viUnlock` were stubs that answered success and enforced nothing,
which is worse than having no locks: callers assume exclusive access and quietly
interleave. Exclusive and shared locks are now enforced, nest, wait out the
caller's timeout, are scoped per instrument rather than per bus, and are
released when a session closes so a crashed client cannot lock an instrument out
for good.

While a lock is held, another client's traffic is left unprocessed until the
lock frees rather than refused — HiSLIP has no "resource locked" message, and
its status queries and lock info still answer so it can find out why it is
waiting.

### Service requests, including the ones a gateway usually cannot see

`viWaitOnEvent(VI_EVENT_SERVICE_REQ)` now works for MAV-driven requests, which a
GPIB bridge normally destroys: HiSLIP has no read request, so the server must
drain the instrument's output queue on its own initiative, and that read is
exactly what clears MAV. The daemon watches the SRQ line across its own read
instead of second-guessing the instrument, so the mask the instrument applies is
the one that decides. MAV in the status byte is reported from message flow, so a
client that polls without enabling service requests still sees that a reply is
waiting.

### Twenty times faster, by accident of a one-line bug

HiSLIP control code 1, `enableRemote` — what `viGpibControlREN(VI_GPIB_REN_ASSERT)`
sends — was a no-op. REN could be dropped by any of the codes that lower it and
never put back, leaving the instrument in local, where an HP 34401A services the
bus about twenty times slower because its front panel is live. 300 queries took
107 s; they now take 5 s.

### Go To Local and Local Lockout

The remote/local control codes were approximated by driving REN, a bus-wide
hammer. There is now an addressed GTL and a universal LLO, so returning one
instrument to its front panel no longer takes every other instrument on the bus
with it. `++loc [pad]` and `++llo` work on the Prologix front-end.

### Adapters

The NI GPIB-USB-HS+ is supported — verified on hardware, and it needed no code
changes. Every supported adapter except the two clones that are byte-identical
to the GPIB-USB-HS has now been exercised on real hardware.

### Behaviour changes worth knowing before upgrading

- Locked-out traffic **blocks** where it previously succeeded. Clients that did
  I/O without taking the lock, while another client held one, will now wait.
- Device clear no longer answers `Interrupted`. It never should have: sent
  without its asynchronous half it can park a strictly conformant client for
  good.
- The server advertises **synchronized** rather than preferring overlapped mode,
  which it never implemented.
- `Device` and `GpibBackend` both gained methods. Out-of-tree implementations of
  either will need updating.

## v0.4.0 — 2026-07-31

Service requests are forwarded to HiSLIP clients as `AsyncServiceRequest`
instead of only being pollable, on both the NI and 82357 backends. The SRQ
forwarder re-polls while the line stays asserted, so a second instrument
asserting while the first still holds the wired-OR line is not missed. The
daemon exits cleanly when its adapter is unplugged rather than answering with
errors it cannot recover from. Several 82357 addressing fixes: device clear
addressed to a listener rather than a talker, and unlisten before addressing a
write.

## v0.3.1 — 2026-07-27

Both binaries report their build version. Homebrew's tap trust model documented,
along with a single-command apt setup.

## v0.3.0 — 2026-07-27

Debian and Ubuntu packaging with a systemd unit and an optional package to
blacklist the kernel GPIB driver, a signed apt repository published to GitHub
Pages, and 64-bit Raspberry Pi OS support with arm64 exercised in CI. The NI
GPIB-USB-HS backend was brought up on real hardware.

## v0.2.1 — 2026-07-20

Adapters can be selected by physical USB port, for hosts with more than one
attached.

## v0.2.0 — 2026-07-20

The daemon binds localhost by default and the Prologix front-end became opt-in.
82357A firmware is bundled and uploaded in-process. `--hislip-default-pad`
became `--default-address` and applies to both front-ends.

## v0.1.0 — 2026-07-18

First release: HiSLIP and Prologix front-ends over the Agilent 82357B.
