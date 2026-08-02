# Changelog

Notable changes per release. The entry for a version is what GitHub publishes
as its release notes, so it is written for someone deciding whether to upgrade
rather than for someone reading the diff — `scripts/release` refuses to tag a
version that has no entry here.

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
