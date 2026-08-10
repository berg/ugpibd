# Roadmap / known gaps

Deliberate simplifications and unfinished work, so that "we chose not to do
this" stays distinguishable from "nobody noticed". Each entry says what the
current behaviour is, why, and what finishing it would take.

Ordered roughly by how likely it is to bite someone. Entries are removed once
they are closed rather than being marked done — `git log` is the record of what
was fixed.

## Guiding rule: no plausible lies

A gap should surface as an error, not as a fabricated success value. A stub that
answers "0" or silently returns nothing is worse than one that fails loudly,
because the caller cannot tell it apart from a working system. Two of these
(`++srq`, `++spoll`) shipped as plausible lies and were fixed in July 2026 —
prefer `bail!("... not supported")` over inventing a value.

---

## 1. No authentication, and TLS/SASL is rejected

**Now:** `StartTLS`, `AsyncStartTLS` and `EndTLS` are answered with a fatal
`SecureConnectionFailed`, and there is no authentication of any kind on either
front-end. Anyone who can reach the port owns the bus: they can drive the
instrument, take an exclusive lock, or assert local lockout.

**Why:** the daemon binds `127.0.0.1` by default, where this does not matter.

**Watch out:** the README documents `--bind 0.0.0.0` for remote access, and
nothing at that point warns that it is unauthenticated. The cheap mitigation is
to say so where the flag is documented; the real fix is HiSLIP 2.0 TLS plus
SASL, which is a substantial piece of work and needs a certificate story.

## 2. Untested adapters: KUSB-488A and MC-USB-488

`3923:725c` and `3923:725d` have never been run. The kernel driver treats them
as byte-identical to the GPIB-USB-HS — same endpoints, readiness handshake, init
and teardown — and unit tests assert their init requests match it exactly, so
buying one adds close to no coverage. Left here so that "nobody has tried"
stays distinguishable from "tried and working".

## 3. Remaining Prologix stubs

`++savecfg` accepts the command and does nothing (`LineResult::Ok`, which writes
no reply). It has no response in the real Prologix firmware, so silence is not
misleading here.

`++status` also returns `Ok`; in Prologix device mode it reports the status byte
the *controller* would present, which is meaningless for a controller-only
implementation. `++mode 0` (device mode) is correctly rejected as unsupported.

`++mode 0` is the one worth revisiting: real Prologix device mode dumps received
data straight to the client, which is the same primitive an unaddressed-listen
capture needs. See `CAPTURE.md` for the design that would close it.

## 4. Secondary addressing

`setup_init` programs the NI adapter with secondary addressing disabled
(`ADR_DISABLE_SAD` / `ADMR_DISABLE_SAD`). The kernel's `ni_usb_write_sad`
supports enabling it. Nothing in the daemon's addressing path (`listen_address`
/ `talk_address`) emits secondary address bytes, and no front-end can request
one. Only matters for instruments that use subaddresses.

## 5. HiSLIP `GetDescriptors` is refused with the wrong error code

Message type 26 is not implemented. It falls through to the catch-all arm and is
answered with a non-fatal Error, control code 1, **"unrecognized message
type"** — but we do recognize it; `MessageType::GetDescriptors` is in the enum.
The refusal misdescribes itself, which is the defect worth recording here
regardless of what the message is for.

It matters because `SUPPORTED_PROTOCOL` is `PROTOCOL_2_0`, so the
InitializeResponse advertises 2.0 and a 2.0 client is entitled to send this.

Finishing it properly needs the spec: 26/27 sit in the 2.0 block alongside the
TLS and SASL messages, and nothing in this repo records what the response
payload is supposed to carry — the vendored message table from lxi-rs gives only
the type numbers. If it turns out to belong to the TLS story, fold this into
item 1 rather than implementing it separately. The cheap interim fix, and the
one consistent with how the lock refusal is reported, is a device-defined error
code with a message saying we recognize the type and do not implement it.

## 6a. Adapter desync (fixed 2026-08-06, kept as a warning)

Removed as an open gap, recorded because the failure mode is invisible and
cost a whole bench session's worth of false negatives before it was understood.

The bulk pipe on both adapters is strictly request/response. One stale reply
desynchronises every transaction after it, and the symptom is not "the adapter
is broken" but *plausible wrong answers*: reads return nothing, captures come
up empty, and an instrument that is working perfectly looks like it is doing
nothing. The NI reports `missing chunk start id`; the 82357 reports
`unexpected response byte 0xfa, expected 0xfb`, or silently delivers its own
`0xfb` response code as if it were instrument data.

Two causes, both fixed:

* **Cancelling a bus read mid-transfer.** The capture loop selected on the
  client socket against the USB read, so a disconnect dropped a future holding
  an in-flight bulk transfer. The adapter still sent its response and nobody
  consumed it. Socket reads are cancel-safe in tokio; USB transfers are not,
  and the two must not be selected against each other. Disconnects are now
  noticed *between* reads.
* **Starting against a pipe left dirty by a dead predecessor.** `init` now
  drains stale responses first. Before that, a fresh daemon failed its first
  init and succeeded on the second — the retry "worked" only because it
  consumed the leftovers, which is why this looked like flakiness rather than
  a bug.

The general lesson for this codebase: never cancel a future that owns a USB
transfer. If a timeout or a disconnect has to interrupt one, the pipe must be
resynchronised afterwards, not merely abandoned.

## 8. 82357 ATN control is implemented but unverified

**Now:** ATN Control on the 82357 is a transcription of the kernel
driver's take-control path (AUX_TCA/AUX_GTS) that has not yet run on that
adapter. Until the phase-9 bench sweep verifies it with the 82357
attached, treat it as unproven.

(Pass Control is not tracked here: refusing it is a signed-off
architectural decision, documented in docs/VXI11.md under "Deviations" —
the daemon is the bus's sole controller.)
