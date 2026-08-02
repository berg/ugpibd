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

## 2. Untested adapter: NI GPIB-USB-HS+

**Implemented, never run.** `3923:7618` has different endpoints (bulk
`0x01`/`0x82`, interrupt `0x83`), a second "analyzer" USB interface we ignore,
and a three-request extra init (`0x48`, `0x4b` LED, `0xf8` — the last with
*interface* recipient).

Unit tests pin both differences (`endpoints_differ_only_for_hs_plus`, and the
init-request comparison against the plain HS), so the shape is right; nothing
has confirmed the hardware agrees.

The KUSB-488A (`3923:725c`) and MC-USB-488 (`3923:725d`) are also untested, but
the kernel driver treats them as byte-identical to the GPIB-USB-HS — same
endpoints, readiness handshake, init and teardown — and a unit test asserts
their init requests match the HS exactly. Buying one adds ~no coverage.

## 3. Remaining Prologix stubs

`++savecfg` accepts the command and does nothing (`LineResult::Ok`, which writes
no reply). It has no response in the real Prologix firmware, so silence is not
misleading here.

`++status` also returns `Ok`; in Prologix device mode it reports the status byte
the *controller* would present, which is meaningless for a controller-only
implementation. `++mode 0` (device mode) is correctly rejected as unsupported.

## 4. Secondary addressing

`setup_init` programs the NI adapter with secondary addressing disabled
(`ADR_DISABLE_SAD` / `ADMR_DISABLE_SAD`). The kernel's `ni_usb_write_sad`
supports enabling it. Nothing in the daemon's addressing path (`listen_address`
/ `talk_address`) emits secondary address bytes, and no front-end can request
one. Only matters for instruments that use subaddresses.

## 5. A fatal error on the async channel is not mirrored onto the sync one

§6.2 requires a desync to be reported on **both** channels before the connection
closes. The sync loop does this: it hands its fatal to the task that owns the
async writer, which sends a copy. The other direction does not — a framing
failure detected on the *async* channel is reported there and nowhere else.

The asymmetry is structural rather than an oversight. The async channel's writer
is already shared behind a mutex so the service-request forwarder can push to
it; the sync writer is owned outright by its loop, which spends its life parked
in a read that is not cancel-safe, so there is nothing to hand a message to.
Closing it means sharing that writer the same way.

Low priority: a desync on the async channel is rare, and the client is told
about it on the channel it was talking on.

## 6. HiSLIP `GetDescriptors` is refused with the wrong error code

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
