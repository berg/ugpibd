# Roadmap / known gaps

Deliberate simplifications and unfinished work, so that "we chose not to do
this" stays distinguishable from "nobody noticed". Each entry says what the
current behaviour is, why, and what finishing it would take.

Ordered roughly by how likely it is to bite someone.

## Guiding rule: no plausible lies

A gap should surface as an error, not as a fabricated success value. A stub that
answers "0" or silently returns nothing is worse than one that fails loudly,
because the caller cannot tell it apart from a working system. Two of these
(`++srq`, `++spoll`) shipped as plausible lies and were fixed in July 2026 —
prefer `bail!("... not supported")` over inventing a value.

---

## 1. Asynchronous SRQ notification — done, except on the 82357B

**Now:** forwarded. A bus SRQ wakes a per-session task that serial-polls and
pushes `AsyncServiceRequest` (type 20) with the status byte — see
`hislip/server.rs`. `viWaitOnEvent(VI_EVENT_SERVICE_REQ)` works.

MAV-driven requests are the awkward case and are handled in
`hislip/instrument.rs`: the daemon's own read is what clears MAV, so `execute`
polls before draining and, failing that, notices an SRQ that fired during the
read and did not survive it. Measured on an HP 34401A: SRQ comes up 5-13 ms
after the write and the poll returns `0x50`, while the daemon's read takes
~350 ms, so the request is always raised inside the window the daemon holds the
bus. The status byte in that second case is reconstructed rather than read —
the only inference left, and a narrow one, since the instrument applied its own
`*SRE` mask when it pulled the line. Deliberately **not** done: parsing `*SRE`
out of the traffic, which would be a cache with no invalidation.

**What is left:** the **82357B** cannot report SRQ, so sessions on it never get
a push (`subscribe_srq` returns `None` and the session says so at debug level).
The hard part already exists: `agilent_82357/usb.rs` runs a permanent interrupt
poller with stall recovery and backoff, and acts only on `AIF_WRITE_COMPLETE_BN`.
`AIF_SRQ_BN` is defined in `protocol.rs` and never tested — the bit is already
arriving and is dropped. The NI path is live and is the model to follow.

## 2. `srq_asserted()` only implemented for the NI backend

**Now:** `GpibBackend::srq_asserted()` defaults to an error
(`"<backend> cannot read the SRQ line"`), and only `ni-usb-hs` overrides it, by
reading the TNT4882 bus status register (`BSR`, `0x1f`, `BCSR_SRQ` = `0x04`).
On the 82357B, `++srq` therefore logs a warning and returns nothing.

**Why:** the 82357B is a TMS9914 and `agilent_82357/protocol.rs` defines no
bus/address status register. Guessing register semantics without hardware to
verify against is the exact failure mode that produced the July 2026 NI bugs.

**To finish:** add the TMS9914 bus-status read, verified on an 82357B. An
alternative is latching `AIF_SRQ_BN` from the existing interrupt poller, but
note that is edge-triggered ("SRQ seen since last check") whereas `++srq` is
specified as a level read ("SRQ asserted right now") — they are not
interchangeable.

## 3. Untested adapters

| Adapter | Status |
|---|---|
| NI GPIB-USB-HS `3923:709b` | Verified on hardware (SR620) |
| Agilent/Keysight 82357B | Supported |
| KUSB-488A `3923:725c`, MC-USB-488 `3923:725d` | Untested, but the kernel driver treats these as byte-identical to the GPIB-USB-HS: same endpoints, readiness handshake, init and teardown. ugpibd shares that path and a unit test asserts their init is identical. Buying one adds ~no coverage. |
| NI GPIB-USB-HS+ `3923:7618` | **Implemented, never run.** Different endpoints (bulk `0x01`/`0x82`, interrupt `0x83`), a second "analyzer" USB interface we ignore, and a three-request extra init (`0x48`, `0x4b` LED, `0xf8` — the last with *interface* recipient). |
| Agilent 82357A `0957:0007` → `0957:0107` | **Implemented, never run.** Distinct firmware image and a different EZ-USB reset address (`cpucs_addr` `0x7F92` vs the B's `0xE600`) — a genuinely separate firmware-upload path. |

## 4. Remaining Prologix stubs

`++llo`, `++loc`, `++savecfg` accept the command and do nothing (`LineResult::Ok`,
which writes no reply). None of them have a response in the real Prologix
firmware, so silence is not misleading here — but `++llo` / `++loc` (local
lockout / return to local) are real bus operations that we simply do not
perform. `GpibBackend` has no local/remote transition beyond `ren()`.

`++status` also returns `Ok`; in Prologix device mode it reports the status byte
the *controller* would present, which is meaningless for a controller-only
implementation. `++mode 0` (device mode) is correctly rejected as unsupported.

## 5. HiSLIP simplifications

Per the module comment in `hislip/server.rs`:

- **TLS / SASL** — `StartTLS`, `AsyncStartTLS`, `EndTLS` are rejected. Fine for
  `127.0.0.1`; a blocker for exposing the daemon on a network.
- **Multi-device** — one bus, addressed per session by the `hislip<N>`
  sub-address.
- **`GetDescriptors`** (type 26) — answered with the generic "unrecognized
  message type" Error rather than a descriptor document. Nothing is known to
  ask for it.

Locking is no longer a simplification: `hislip/lock.rs` implements exclusive and
shared locks with timeouts, nesting and release-on-disconnect, enforced against
the traffic of sessions that do not hold them. Two things about it are worth
knowing:

- **Refusal, not delay.** A locked-out Data/Trigger/device-clear message is
  answered with a non-fatal Error in the device-defined code range (128), which
  is the VISA model — `VI_ERROR_RSRC_LOCKED` is immediate, and you are expected
  to take the lock before doing I/O rather than queue behind someone else's.
  HiSLIP defines no "locked" reply of its own, so the code is ours.
- **Clients may not surface it.** pyvisa-py discards unexpected message types
  while waiting for `Data`, so a refused read presents as a timeout rather than
  as `VI_ERROR_RSRC_LOCKED`. The server's half is done; making it legible is a
  client-side change.

## 6. Secondary addressing

`setup_init` programs the NI adapter with secondary addressing disabled
(`ADR_DISABLE_SAD` / `ADMR_DISABLE_SAD`). The kernel's `ni_usb_write_sad`
supports enabling it. Nothing in the daemon's addressing path (`listen_address`
/ `talk_address`) emits secondary address bytes, and no front-end can request
one. Only matters for instruments that use subaddresses.
