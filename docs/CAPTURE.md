# Bus capture: talk-only sources, plotters and printers

**Status:** implemented and working on both instruments it was written for.
See §0 for what to use; §1-13 are the design; §14 is the bench record,
including what was got wrong on the way.

## 0. What this is, and which mode an instrument needs

An instrument that plots or prints to GPIB does not answer queries. It drives
the bus at a *listener*, and until a listener exists it emits nothing at all —
so the usual "read from the instrument" model does not apply and returns
silence no matter how long you wait.

There are two ways instruments do this, and which one applies is a property of
the instrument, not a choice:

| The instrument | Mode | Verified with |
|---|---|---|
| goes talk-only and drives the bus itself | `++lon 1` / `--listen-only` | HP 53310A — 558 rows of PCL raster (§14.12) |
| addresses a plotter at a configured address | `++dev <addr>` / `--listen-address` | SRS SR620 — 4408 bytes of HP-GL to address 5 (§14.14) |

Both stream raw bytes to `--capture-port`. The daemon does not parse, frame or
render anything: where one page ends is a question only a client with a format
parser can answer (§5).

**Neither instrument needed a plotter persona.** Both send and stop, asking the
plotter nothing — so the `OI`/`OP`/`OE` emulation §8 anticipated has no
customer here and was never built. That was not knowable until something
existed at the plotter address to be asked.

**When a capture is silent, reach for `++lines` first.** It reports the eight
bus control lines, which is what separates "the instrument is not transmitting"
from "the instrument is transmitting and we are not receiving it" — two states
that look identical from the data path and that cost this project most of a
bench session to tell apart.

### Known-good invocations

```
# talk-only source (53310A: set TALK ONLY on the front panel first)
ugpibd --enable-prologix --capture-port 1235
printf '++mode 1\n++lon 1\n' | nc -q1 localhost 1234
nc localhost 1235 > print.pcl

# instrument that addresses its own plotter (SR620 with plotter address 5)
printf '++mode 1\n++dev 5\n' | nc -q1 localhost 1234
nc localhost 1235 > plot.hpgl
```

### What is not done

* The 82357 backends. Listen-only reaches a genuinely ready listener there but
  nothing is collected once it is there (§14.15); device mode is
  refused outright.
* Rendering. `contrib/fixtures/` holds real captures of both formats for
  whoever writes it.

---

## 1. How an instrument talks to a plotter

Three distinct bus mechanisms. Which one a given instrument uses decides
entirely how much work it is to support, so this is the first thing to
establish per instrument (§7).

**Case 1 — talk-only / listen-only.** No controller involved. The instrument is
set to "talk only" and drives the bus unaddressed; the plotter is set to
"listen only" and accepts every data byte. Both sides are unaddressed, so the
plotter's configured address is irrelevant.

**Case 2 — controller-mediated third-party transfer.** The controller addresses
the instrument as talker and the plotter as listener, then drops ATN and gets
out of the way. Bytes flow instrument→plotter without passing through the
controller. The instrument's configured "plotter address" is irrelevant here
too — it has no idea who is listening.

**Case 3 — the instrument is the controller, and we are a device.** We never
become a controller at all: no system control, no IFC, no REN. We come up as a
plain addressable device at the plotter's address and wait. The instrument
addresses us as listener, sends HP-GL, and addresses us as talker when it wants
a query answered. **This is the common case in practice**, not the exotic one
(§13).

**Case 4 — pass control.** As case 3, but the bus already has a controller which
hands over with TCT and takes control back afterwards. Distinguishing this from
case 3 matters: case 3 needs only that we are a device on a bus whose controller
happens to be the instrument, while case 4 additionally needs the TCT handshake
on both sides. The prior art in §13 suggests case 4 is rare — emulators that do
not implement pass control at all still work with the HP analyzers of exactly
the vintage that supposedly needs it.

Cases 3 and 4 need us to be an addressable *device*. Cases 1 and 2 need only
that we accept bytes.

The consequence for this daemon, and it is not a small one: in case 3 we are
**not** system controller, so the "we can always pulse IFC to recover the bus"
guarantee relied on below does not apply. `init()` currently requests system
control unconditionally (`gpib.rs:193`, `request_system_control()` at
`gpib.rs:198`). Device mode needs a way to come up without ever doing so.

---

## 2. The primitive

Unaddressed listen, plus a streaming read that does no addressing.

On the TMS9914 this is the `lon` auxiliary command (`AUX_LON = 0x9`,
`linux/linux-7.0/drivers/gpib/include/tms9914.h:262`; already named in
`src/backend/agilent_82357/protocol.rs:112`, where it is currently issued only
as part of init's aux sweep and then immediately undone by the chip reset). The
TNT4882 has the equivalent. In this state the chip accepts every data byte on
the bus regardless of addressing.

Proposed trait surface, added to `GpibBackend`:

```rust
/// Where a capture's bytes come from.
pub enum CaptureSource {
    /// Unaddressed listener: accept every data byte on the bus, whoever is
    /// talking. Case 1, and the only option when we are not controller.
    ListenOnly,
    /// Address `pad` as talker and ourselves as listener, then drop to
    /// standby. Case 2. Requires us to be CIC.
    Talker(u8),
}

async fn capture_start(&mut self, src: CaptureSource) -> Result<()>;
/// Read the next chunk. `Ok((bytes, true))` means EOI was seen on the last
/// byte. An idle period returns `Ok((vec![], false))` rather than an error —
/// silence is normal in a capture, unlike in a request/response read.
async fn capture_chunk(&mut self, max: usize, timeout_ms: u32) -> Result<(Vec<u8>, bool)>;
async fn capture_stop(&mut self) -> Result<()>;
```

Per the roadmap's "no plausible lies" rule, the default implementations must
`bail!`, not return empty — a backend that cannot capture must be
distinguishable from a quiet bus.

Symmetrically, `AUX_TON = 0xa` gives unaddressed *talk*. That is not needed for
any of the three cases, but it is what makes the hardware-in-the-loop test in
§9 possible with no instrument at all, so it is worth adding at the same time.

There is a GPL reference for both in-tree: `check_my_address_state()`
(`tms9914/tms9914.c:630-662`) toggles `AUX_LON`/`AUX_TON` with and without the
`AUX_CS` set bit as addressing changes, and the nec7210/TNT family carries the
same thing as `HR_LON`/`HR_TON` bits in `ADMR`
(`include/nec7210_registers.h:134-135`). Nothing here needs to be guessed at.

### 2.1 Reading the bus lines

Both chips expose the live GPIB lines in a bus-status register with an
identical bit layout, confirmed against the kernel tree:

| Line | TMS9914 `BSR` | TNT4882 `BSR` |
|------|---------------|---------------|
| REN  | `0x01` | `0x01` |
| IFC  | `0x02` | `0x02` |
| SRQ  | `0x04` | `0x04` |
| EOI  | `0x08` | `0x08` |
| NRFD | `0x10` | `0x10` |
| NDAC | `0x20` | `0x20` |
| DAV  | `0x40` | `0x40` |
| ATN  | `0x80` | `0x80` |

(`tms9914.h:236-243`; `agilent_82357a_line_status()` at
`agilent_82357a/agilent_82357a.c:1020` reads exactly this. The NI side is
already fully named in `src/backend/ni_usb_hs/protocol.rs:60-67`; the 82357
side names only `BSR_SRQ` today at `protocol.rs:96`.)

So a `bus_lines()` method returning all eight is a handful of lines on both
backends, and it is what makes §7 possible.

---

## 3. What this gets us

Three consumers. The confidence ordering below was a guess and the first two
turned out backwards — plot capture works on both instruments, while talk-only
*text* logging still has no instrument here that does it:

1. **Talk-only text logging.** An instrument in talk-only pushing readings
   continuously, streamed to a socket. This works on instruments with *no*
   remote programming at all, which nothing else in the daemon can reach.
2. **Plot capture**, cases 1 and 2. The bytes are HP-GL; rendering is a
   separate concern (§6.3).
3. **Passive tap** while some other controller owns the bus. Worth stating the
   limit up front so it is not oversold: `lon` delivers *data* bytes only.
   Command bytes (sent with ATN asserted) are interpreted by the chip rather
   than handed to us, so this is a data sniffer, not a protocol analyser.

---

## 4. What in the current code is wrong for this

Not merely missing — these are behaviours that will actively break a capture.

**4.1 Timeout recovery pulses IFC.** `recover_from_stall()`
(`src/backend/agilent_82357/gpib.rs:397`) ends with `self.ifc()`, and it runs on
every read timeout. During a capture that is destructive in exactly the wrong
moment: IFC knocks a talk-only instrument out of TON mid-plot, so the recovery
path destroys the transfer it was trying to salvage. Capture needs its own
recovery that aborts the USB transfer and stops there.

**4.2 Reads are one-shot, not streaming.** The 82357 path issues a single
`read_bulk(max_len + 1)` (`gpib.rs:295`); the NI path sizes one response buffer
up front — `resp_cap = (max_len / 30 + 1) * 0x20 + 0x20`
(`src/backend/ni_usb_hs/mod.rs:314`) — and clamps `max_len` to
`MAX_TRANSFER_LEN`, which is `0xffff` (`protocol.rs:193`), because the
adapter's length field cannot express more. A plot is tens of KB arriving over
tens of seconds and can plausibly exceed 64 KiB outright. `capture_chunk` must
loop, delivering incrementally.

**4.3 `set_timeout` is per-backend, not per-operation.** A capture wants tens of
seconds; a HiSLIP query sharing the same adapter wants three. Today one
clobbers the other. Either capture saves and restores it under the lock, or
timeout moves into the call signature.

**4.4 A capture holds the whole bus.** Every method takes `&mut self` behind the
single `Arc<Mutex<dyn GpibBackend>>` (`src/backend/mod.rs`), so a capture in
progress blocks every HiSLIP and Prologix operation for its duration. That is
arguably *correct* — the bus really is busy — but it presents as a hang. It
needs to be a distinguishable state with an error that says so ("bus held by
capture since T"), and the capture must be cancellable.

**4.5 REN, and the gotcha that will cost an hour.** `init()` asserts REN
unconditionally (`gpib.rs:193`). REN alone is harmless, but once any session has
addressed an instrument as listener it is in remote — and then you press PLOT on
the front panel and nothing happens. Capture setup should call the existing
`go_to_local()` (`gpib.rs:260`) for the source instrument, and there should be a
way to bring the daemon up without asserting REN at all.

**4.7 The RFD holdoff makes an idle daemon block talk-only talkers.** This is
the one that actually stops capture working, and it was found on the bench.
`setup_init` programs `AUX_HLDI` ("rfd holdoff immediately",
`ni_usb_hs/protocol.rs:404` and `:549`) plus `AUXRA | HR_HLDA` (`:536`), exactly
as the kernel driver does. That is correct for the controller role — it is how
bytes are not dropped between reads — but it means the adapter holds NRFD
asserted whenever it is not actively reading.

A talk-only device therefore sees a listener that never becomes ready, and
refuses to transmit. The HP 53310A says so in as many words: **"no ready
listeners?"** — not *no listeners*, but no **ready** ones. An idle ugpibd does
not merely fail to receive a talk-only source; it actively prevents it from
transmitting, and that applies to the whole bus, not just to us.

Capture must therefore release the holdoff for its duration, not only put the
chip in listen-only.

**4.6 `++mode 0` is rejected** (`src/prologix.rs:214-217`, and ROADMAP item 3).
Prologix device mode dumps received data straight to the client, which is
precisely these semantics. This work closes that gap rather than adding a
parallel concept beside it.

---

## 5. Framing is the client's problem

A capture stream has no request boundaries, so "when is this plot finished?"
has no single answer:

- **EOI on the last byte** — clean when the instrument sets it, and many do not.
- **Idle timeout** — always works, always arbitrary.
- **Format-level** (HP-GL `PG`, or `SP0` pen-away) — correct, and only the
  client can do it.

An earlier draft made this a daemon-side `--until eoi | idle:<ms> | never`
policy. That was wrong, and inconsistent with §6.3's own argument: if rendering
stays out of the daemon because a bus driver has no business knowing HP-GL,
then neither does boundary detection, which needs the same knowledge. The
daemon cannot do the job well anyway — it has only timing and EOI, while the
client has the instruction stream. VK2BEA's emulator does exactly this
client-side, as its inactivity-based "auto clear".

So: **the daemon streams bytes and never decides where a plot ends.** A client
connects, reads until it stops caring, and splits the stream itself.

One thing the daemon knows that a byte stream cannot express: **EOI**. If an
instrument does set EOI at the end of a plot, that is a bus-level fact the
client cannot recover. Whether that needs an out-of-band marker depends on
whether real instruments actually do it — which `contrib/bus_capture_probe.py`
answers directly. Measure before designing anything for it.

The same goes for the read-termination reasons the adapter already reports
(`agilent_82357a.h:52-60`): `ATRF_UNADDRESSED`, `ATRF_ATN`, `ATRF_IFC`, `ATRF_DEAD_BUS`. None are
needed for framing, but "the capture ended because another controller asserted
ATN" is exactly what you want when a plot comes out truncated. Log them; do not
put them in the stream.

---

## 6. Interface

### 6.1 Shape

A capture is a *stream*, which neither existing front-end models well.

**HiSLIP is a worse fit than it first appears, but not for the obvious reason.**
It is tempting to say it is request/response and cannot push — that is wrong.
HiSLIP has no read request at all; the server emits `Data`/`DataEnd` and the
client's `viRead` consumes whatever arrived (`hislip/instrument.rs:52-55`). The
transport is already server-push.

The real obstacles are:

- **`MessageID` correlation.** Every server response quotes the id of the client
  message it answers (`hislip/server.rs:895-924`), and our MAV reporting keys
  off `last_message_id` (`server.rs:350-359`). Unsolicited capture data has no
  request to quote and would desynchronize exactly that.
- **Our server only reads the bus as a side effect of a write, and only when
  the written command contains `?`** — `let expect_response = cmd.contains(&b'?')`
  (`server.rs:761`). There is no read-without-write path at all. Since a
  talk-only instrument does not listen, there is no write to hang a read off,
  so the HiSLIP front-end cannot capture from one *at all*. Measured, not
  argued: see §14. This is an implementation choice rather than a spec
  requirement, but it is a hard blocker today.
- **No flow control** for an unbounded stream into a client that is not reading,
  `viRead` timeouts firing constantly on a quiet talk-only bus, and VISA read
  semantics expecting a message terminated by END — which a capture has none of.

It *could* be done two ways, both poor: a magic write the client polls with and
the server answers from a capture buffer (protocol-legal, turns a stream into
polling), or unsolicited `Data` messages (lenient clients would accept them; it
breaks the id correlation, and shipping it would be the kind of plausible lie
the roadmap warns about). The idiomatic version is to buffer, raise
`AsyncServiceRequest` — a legitimate server push we already implement
(`server.rs:1056`) — and let the client read.

**Prologix, by contrast, is the right shape.** Device mode dumps received data
straight to the client with no framing and no correlation, which is a capture
stream exactly. Its only problem is ergonomics: a 40 KB binary plot on a
line-oriented protocol.

The asymmetry is worth stating plainly, because it is the reason for the socket
below: HiSLIP's session model assumes a bounded exchange with one addressed
instrument. Talk-only has no request, no addressee and no end.

Proposal: a third listener, off by default, `--capture-port`. On connect it
streams raw bytes and nothing else — no framing, no headers, no boundaries — so
that `nc localhost 1235 > plot.hpgl` is the whole client. Configuration (which
source to capture from) is a per-daemon flag, not an in-band protocol, so the
socket stays trivially composable.

`++mode 0` on the Prologix front-end is then a second transport for the same
bytes, which is what real Prologix hardware does. Worth having for
interoperability rather than as the primary interface: KE5FX's 7470 emulator
supports Prologix in device mode for device-initiated plots (§13.1), so
speaking that dialect faithfully points a mature HP-GL renderer at ugpibd
instead of writing one. It reaches Prologix over a COM port, so it needs a
serial bridge — a reason to keep it in view, not to design around it.

Note the receive direction needs no escaping: real Prologix escapes ESC, CR, LF
and `+` only on data being *sent* to an instrument, and passes received data
through unmodified. (This daemon implements no escaping in either direction —
`src/prologix.rs` — which is a latent gap for binary writes, unrelated to
capture.)

### 6.2 One client

Same policy as the rest of the daemon: one capture at a time, second connection
refused with a reason. Two clients cannot both consume one byte stream.

### 6.3 Rendering stays out of the daemon

Rendering does not belong in a bus driver. The daemon emits bytes; a separate
filter does the rendering, and is unit-testable against captured files with no
hardware. Keeping it separate means a bad plot is diagnosable as "we captured
the wrong bytes" versus "we drew them wrong".

**There is more than one format, which an earlier draft got wrong.** This
section assumed HP-GL throughout. The bench says otherwise:

| Instrument | Emits | Renderer |
|---|---|---|
| SR620 | HP-GL vectors (assumed; not yet captured) | `SP/PU/PD/PA/PR/LB/CI/LT` → SVG |
| 53310A | **PCL raster** (captured, §14.12) | `ESC*r…S` / `ESC*b…W` rows → PNG |

They share nothing but the transport. That is an argument for the split this
section already proposes rather than against it — the daemon stays format-blind
and each decoder is a separate, testable filter — but "write an HP-GL renderer"
is now only half the job, and the half with no captured sample yet.

---

## 7. Diagnose mode: which case is each instrument in?

With `bus_lines()` (§2.1), the adapter can classify an instrument
automatically. Sit in listen-only, ask the operator to press PLOT, and watch:

| Bytes arrive | ATN asserted | Conclusion |
|---|---|---|
| yes | no | **Case 1.** Talk-only. Already done — nothing further needed. |
| no | no, instrument idle | **Case 2.** Waiting to be addressed to talk; `read()` nearly covers it today. |
| no | **yes** | **Case 3.** The instrument is trying to take control. Needs §8. |

This should be built *first*. It is small, it needs only `bus_lines()` plus
`ListenOnly`, and its answer decides whether the expensive part (§8) is needed
at all.

Table to fill in from the bench:

| Instrument | Plotter output | Case | Notes |
|---|---|---|---|
| SR620 | yes | **3** | Has a plotter-address setting (set to 5). Addresses the plotter itself; dumps nothing to its output queue (§14.6) |
| 53310A | yes | **1** | No plotter-address setting at all, and refuses to print unless in talk-only: *"print ignored, not in talk only mode"* (§14.9) |
| 34401A | no | — | no plot function |
| 53132A | no | — | no plot function |

Both guesses in an earlier draft of this table were wrong. The SR620 was
predicted to be case 1 or 2 because SRS instruments generally lack a controller
subset; it is case 3. The 53310A was predicted to be case 3 or 4 on HP-house-
style grounds; it is case 1. Neither prediction survived contact with the
instrument, and the front panel settled both in minutes. Do not guess this —
measure it.

---

## 8. Case 3: device mode, and why it is a separate project

**Superseded in part, and smaller than it looks.** The device-mode half of this
section is built and working (`++dev`, §14.14). The *persona* half was never
needed: neither instrument on this bench asks the plotter anything, so nothing
answers `OI`/`OP`/`OE` and nothing has missed it. Read the rest as the
reasoning that led there, not as outstanding work.

To satisfy an instrument that insists on addressing the plotter itself, we must
stop being a controller and become an addressable device: respond when
*someone else* addresses us as listener, accept HP-GL, and talk back when
addressed as talker. Then a plotter persona on top — enough of a 7470A/7475A
that the handshake completes (`OI` → model string, `OP` → hard-clip P1/P2
limits, `OE`/`OS` → no error, `OA`/`OC` → position). A wrong `OP` yields a
correctly drawn plot at the wrong scale.

The silicon supports it. The TNT4882 has full device mode — it is how NI-488.2
non-controller mode works — and this codebase already names the `IBSTA_LACS` /
`IBSTA_TACS` bits (`src/backend/ni_usb_hs/protocol.rs:132-133`) that signal
"am I currently addressed", plus `CMDR_CLRSC` to give up system controller. The
TMS9914 has `AUX_RQC = 0x11` (request control) and `AUX_RLC = 0x12` (release
control) — `rqc` is already named at `src/backend/agilent_82357/protocol.rs:118`
— and one `ADR` register, which is fine: set it to the plotter's address for the
duration of the plot.

**The chip-level mechanism is well referenced.** `tms9914.c` implements device
mode in full: `HR_MA` ("have been addressed", line 739), `HR_UNC` plus the
command-pass-through register `CPTR` to interpret command bytes sent by
*another* controller (line 743), `HR_DCAS` and `HR_GET` for device clear and
trigger received (lines 793, 799), `HR_APT` for secondary addressing (line
805), `HR_SPAS` for answering someone else's serial poll (line 730), and
`HR_IFC` clearing our own CIC bit when another controller asserts IFC (line
788). That is the whole state machine, GPL, in the tree.

**The constraint is per-adapter, and it is not symmetric.** Every one of those
is an *interrupt* bit in ISR0/ISR1 that clears on read. Over USB we only learn
about them if the adapter's firmware forwards them, and the two adapters differ
sharply:

- **NI GPIB-USB-HS: plausible.** Its firmware returns an `ibsta` word on every
  operation carrying `LACS | TACS | DCAS | DTAS | CIC | ATN | REM | LOK`
  (`ni_usb/ni_usb_gpib.c:317-347`), and `ni_usb_set_interrupt_monitor()` asks
  the adapter to notify asynchronously on selected bits. "You have been
  addressed as listener", "device clear received", "trigger received" all have
  a real event path to the host. This codebase already names `IBSTA_LACS` /
  `IBSTA_TACS` (`src/backend/ni_usb_hs/protocol.rs:132-133`).
- **82357B: unproven.** `agilent_82357a_request_system_control()` returns
  `-EINVAL` when asked to *release* control (`agilent_82357a.c:778-779`), so the
  kernel driver does not merely try and fail — it refuses to try. Whether the
  hardware would comply is untested; `AUX_RLC` exists on the chip and the
  `SYSTEM_CONTROLLER` bit lives in a register this daemon already composes
  itself (`hw_control_bits`, `gpib.rs`), so there is no evidence either way.

`DESIGN.md` §1.2 asserts the adapters cannot do device mode at all. That is now
false for the NI, which runs an SR620 plot through `++dev` (§14.14), so the
claim should be narrowed rather than cited.

Note also that `ibgts()` in the vendored tree
(`common/iblib.c:136-142`) returns `-EINVAL` unless the board is CIC, so the
*controller-side* half of pass control has no in-tree example even on NI.

The one thing that genuinely de-risks it: we hold `SYSTEM_CONTROLLER`. If pass
control goes wrong and the instrument never returns TCT, pulsing IFC makes us
CIC again unconditionally. That is a hardware guarantee, not a protocol
agreement, so the bus can always be recovered.

Do this only if §7 says an instrument we actually own requires it.

---

## 9. Testing

The useful property: **this is testable with two adapters and no instrument.**
Adapter A in talk-only (`AUX_TON`) dumps a canned HP-GL file; adapter B in
listen-only captures it. That exercises the unaddressed-listen path, the
chunked streaming read, the framing policy, and the renderer end to end, with
both ends under our control — a better test than a real instrument, because the
expected bytes are known exactly.

What it cannot tell us is whether a particular instrument's idiosyncratic
handshake works. That still needs the SR620 and the 53310A on the bench.

Only one adapter may be system controller; the second needs SC cleared
(`CMDR_CLRSC` on NI, the `SYSTEM_CONTROLLER` bit in the 82357's hardware
control word). Both paths exist already.

---

## 10. Phasing

1. `bus_lines()` on both backends, and the diagnose mode of §7. Small, and it
   decides everything downstream.
2. `CaptureSource::ListenOnly` + `AUX_TON` + chunked `capture_chunk`, with the
   §4 fixes (capture-specific recovery, per-operation timeout, cancellable
   capture holding the bus lock).
3. The capture socket front-end, and `++mode 0` as an alias for it.
4. `hpgl2svg` as a separate filter, unit-tested against captured files.
5. `CaptureSource::Talker(pad)` for case 2 — a small delta on the existing
   `read()`.
6. Device mode and the plotter persona (§8) — the actual target, per §13.3, not
   a conditional extra. Needs `init()` to be able to skip system control (§1),
   `bus_lines()` from step 1, and the NI adapter's `ibsta` event path.
7. Pass control (case 4) only if §7 turns up an instrument that demands it.

Steps 1–5 are useful on their own and do not depend on device mode landing:
talk-only logging, the passive tap, and case-2 capture all work from the
controller side. They are also the honest fallback if the 82357B turns out to be
capture-only.

---

## 11. Open questions

- Does the 82357B deliver bytes that arrive while the chip is an unaddressed
  listener, or only as part of a host-initiated addressed transfer? No kernel
  reference answers this — the in-tree drivers never set listen-only — and it is
  still the open question behind §14.15.
- Does `lon` survive the init sequence's chip reset, or must it be re-issued
  after? (`gpib.rs:88` issues `AUX_LON` and then `AUX_CHIP_RESET` in the same
  batch.)
- Is there a sane way to expose "capture is holding the bus" to a HiSLIP client
  that is blocked on it, short of just timing out?

---

## 13. Prior art

Two existing emulators, both worth reading before writing any of this.

### 13.1 KE5FX GPIB Toolkit — 7470A emulator (Windows)

<https://www.qsl.net/ke5fx/gpib/7470.htm>

Splits plotting into exactly the two paradigms this document calls case 1/3 and
case 2, and names them usefully:

- **Device-initiated** — the operator presses PLOT on the front panel and the
  emulator waits passively. Needs the emulator to be a device at the plotter's
  address, *or* in listen-only mode with no address at all.
- **Host-requested** — the PC is controller, queries the instrument and pulls
  the plot back without touching the front panel.

Confirmations that matter here:

- **Listen-only is a real, needed mode**, exposed as an explicit "no assigned
  plotter address (listen-only)" option, and required by some instruments (the
  Tektronix 49xP series is called out). §2's `ListenOnly` is not a nicety.
- **Prologix maps onto both**: device mode does device-initiated plots,
  controller mode (firmware ≥ 3.1) does host-requested. Direct precedent for
  treating `++mode 0` as the device-side entry point (§4.6).
- **Talk-only can monopolise the bus** — some instruments in talk-only prevent
  any other GPIB communication. Worth surfacing as a documented consequence
  rather than letting it present as a wedged daemon.

Concrete HP-GL query responses, which saves guessing at §8's plotter persona:

| Query | Response |
|---|---|
| `OI` | `7470A` |
| `OH` | `0,0,10000,7500` |
| `OP` | `0,0,10000,7500` |
| `OE` | `0` |
| `OO` | `0,1,0,0,0,0,0,0` |
| `OF` | `40,40` |

All of them are optional and individually suppressible in its config, which
implies many instruments never query at all — so a minimal persona is likely to
work before any of this is polished.

### 13.2 VK2BEA HPGL-Plotter (Linux, GTK4, linux-gpib)

<https://github.com/VK2BEA/HPGL-Plotter>

The closest thing to a reference implementation for us: Linux, linux-gpib, pure
device mode, renders to PDF/SVG/PNG. Two findings carry real weight because they
are empirical rather than argued:

- **It does not implement pass control at all**, and is nonetheless verified
  against the HP 8753C, 8713B, 8595E, **8568B**, 54100, 4145A and R&S UPL. That
  is the strongest available evidence that case 4 is rare and case 3 is the one
  to build.
- **It reports the NI GPIB-USB-HS working and the Agilent 82357A/B not**, on the
  grounds that the 82357 "only functions as a system controller". Treat this as
  true-of-linux-gpib rather than true-of-the-hardware: see §8, where that claim
  is traced to a single `return -EINVAL`.

It also settles §5's framing problem by putting it where it belongs: an "auto
clear" that detects plot completion by inactivity, client-side, in the process
that already has the HP-GL parser.

### 13.3 What this changes

1. Device mode is the **main** path, not the exotic one. §10's phasing had it
   last and conditional; it should be the target, with listen-only capture as
   the useful thing that lands first on the way there.
2. The instrument-as-controller case does not need TCT. Build case 3; treat
   case 4 as unimplemented until an instrument here demands it.
3. `init()` unconditionally taking system control is now a blocker rather than a
   wrinkle, since case 3 requires never taking it.
4. Whether the 82357B can join in is a live, cheap question rather than a closed
   one — and answering it either way is a result worth publishing back, since
   the current public answer rests on a driver policy nobody has tested against
   the silicon.

---

## 14. Bench results, 2026-08-06

First hardware session. NI GPIB-USB-HS+ (`0x7618`) on macOS, one instrument on
the bus: an HP 34401A, put into TALK ONLY from the front panel.

### 14.1 A retracted result, and why it fooled the controls

An earlier revision of this section claimed talk-only capture already worked
through `++read`, at 6.5 readings/s. **That was wrong.** The instrument was not
in talk-only at the time; it was an ordinary addressed talker at pad 23 with a
measurement left running by an earlier `INIT`/`FETC?` that had been killed
mid-sequence, so it had readings to hand out on every read.

The controls did not catch it, and it is worth recording why. "`++read` at an
empty address times out, `++read` at pad 23 returns data" was read as proof the
probe could fail. It was actually the *disproof* of the hypothesis: if a
talk-only device were driving the bus regardless of addressing, every address
would have returned data. One address working and nine timing out is precisely
what an addressed talker looks like. The control was informative and was
interpreted backwards.

A power cycle settled it. On reboot the instrument reports address 31 (talk
only), and then:

```
*IDN?  at 23, 5, 11   -> timeout        (it does not listen)
++read at 23, 5, 11   -> timeout        (10 s, patient)
++spoll at 23         -> 0              (nothing there)
++addr 31             -> error: invalid address: 31
```

### 14.2 The real result: talk-only is unreachable without the primitive

This is positive evidence for §2 rather than against it. A talk-only device is
**not addressable by construction**: HP expresses the mode as "address 31", and
31 is not a primary address at all — `0x5F` is the untalk command. `++addr`
rejecting it (`prologix.rs:116`, accepting 0-30) is correct behaviour, not a
bug. There is no address to point `read()` at, so no amount of front-end work
reaches a talk-only source.

Receiving from one requires becoming an unaddressed listener — `AUX_LON`, §2 —
which this daemon does not have. So talk-only logging needs the new backend
primitive after all, and now that is a measured fact rather than a design
argument.

### 14.3 What is still ambiguous, and what would resolve it

`read()` addresses *us* as listener (`MLA(0)`) and drops to standby, so we were
a valid listener throughout and still received nothing. Two explanations fit:

1. The 34401A in talk-only was not driving the bus at all (idle, untriggered).
2. It was driving, and an addressed read is the wrong operation to catch it.

These cannot be told apart from the data path alone. `bus_lines()` (§2.1)
separates them immediately — DAV and NRFD/NDAC activity says whether anything
is being handshaked. That is another reason it is step 1 in §10.

### 14.4 Incidental, and unaffected by the retraction

- 512 readings via `FETC?` returned 8192 bytes in one transfer, exact and
  untruncated. The multi-KB read path is healthy; >64 KiB (§4.2) is still
  untested, since a 34401A cannot produce that much in one response.
- A bare read over HiSLIP produces no GPIB traffic at all, because the server
  reads only when the written command contains `?` (`server.rs:761`). That
  finding stands and is independent of the instrument state — see §6.1, and
  the MAV-based alternative below.
- `expect_response = cmd.contains(&b"?")` is a heuristic with two failure
  modes: a false positive on a `?` inside a string argument
  (`DISP:TEXT "why?"` provokes a read that times out), and a false negative on
  any unsolicited output. The instrument already tells us the right answer —
  serial poll returned `0x10` (MAV) when it had data — and `instrument.rs:75-89`
  already serial-polls around the read for the SRQ logic. Deciding *whether* to
  read from MAV would be strictly better than sniffing for `?`.

### 14.5 Backpressure, not buffering

GPIB handshakes byte by byte, so a talker with no listener stalls on DAV/NRFD
rather than overflowing. There is no queue to overrun on the bus, and none in
the daemon today: one read per client request, bounded by `max_read = 65536`,
returned straight to the caller.

The capture socket inherits a choice, though. A slow client means either
buffering in the daemon (unbounded memory) or declining to read (backpressure
onto the instrument). Backpressure is right, but it is not free: a stalled
talker holds NRFD/NDAC and blocks the **whole bus**, so a slow capture client
can wedge every other instrument on it. Document that as a consequence of the
design rather than letting someone discover it.

### 14.6 C3 answered: the SR620 does not dump, so §8 is required

SR620 (`StanfordResearchSystems,SR620,03715,1.48`) at pad 16, alone on the bus,
plotter mode on, plotter address set to 5. PRINT pressed several times during a
90-second window in which the daemon read pad 16 continuously.

**Zero bytes.**

The negative is real, not a broken rig. Three controls:

- Baseline before the run: `*IDN?` at 16 answered; `*IDN?` at pad 9 timed out,
  so the probe could fail.
- After the run the SR620 was still alive and answering, so the bus had not
  wedged.
- The **same** `++read eoi` loop, pointed at a queued `*IDN?` response,
  retrieved all 41 bytes. The loop that returned nothing for the plot does
  return data when data exists.

The instrument does not think it failed, either: `*ESR?` was `192` — `PON` plus
`URQ` (user request, i.e. the button press registered) with **no** command,
execution or query error bits set. It believes it did something.

So the SR620 does not put its plot in the output queue, and no amount of
controller-side reading reaches it. Cases 1 and 2 are ruled out for this
instrument. The plot is going somewhere we are not: it is addressed at the
plotter, and we are not the plotter.

This is the result §7 was built to get, and it says **build §8**. Passive
capture is not sufficient for the plotting instrument actually on this bench.

### 14.7 What it does not yet say

Which of case 3 or case 4 this is remains open, and the distinction matters
because case 4 needs a TCT handshake on both sides.

Note one constraint that narrows it: only the controller-in-charge may pass
control, and we never sent TCT, so the SR620 **cannot** have become CIC during
that window. Either it is waiting for a listener at address 5 before it will
emit anything, or it expects to be passed control and quietly gives up when it
is not. Both are consistent with the evidence.

`bus_lines()` (§2.1) separates them: if the SR620 asserts ATN when PRINT is
pressed, it is trying to take control. That is one more reason it is step 1.

The next experiment after that is the direct one — put the adapter at address 5
and stop taking system control, so the SR620 can address us. That needs a
`--my-address` flag (`my_pad` is already plumbed through `init()` at
`backend/mod.rs:36` and hardcoded to 0 at the call site) plus §1's
never-take-system-control path.

### 14.8 The 53310A is case 1, and says so out loud

`HEWLETT-PACKARD,53310A,0,3235` at pad 12. Two things it told us directly:

1. With the daemon holding REN, pressing PRINT gave **"key ignored, analyzer in
   remote"** — §4.5 exactly, in the field, costing about the hour it was
   predicted to cost. Capture setup must return the instrument to local, or the
   daemon must not take REN at all.
2. In local, pressing PRINT gave **"print ignored, not in talk only mode"**.

So the 53310A does not address a plotter — that is why it has no plotter-address
setting — and will not emit anything until it is in talk only. It is case 1, and
capturing from it requires unaddressed listen (`AUX_LON`), the same primitive
the 34401A showed is unavoidable (§14.2).

### 14.9 Both primitives are required, and each now has a customer

This is the useful shape of the session. The two plotting instruments on this
bench are in *different* cases and need *different* halves of this design:

| Instrument | Case | Needs | Testable today |
|---|---|---|---|
| 53310A | 1 | `AUX_LON` unaddressed listen (§2) | no |
| SR620 | 3 | device mode at the plotter address (§8) | no |
| 34401A (talk-only) | — | `AUX_LON` (§14.2) | no |

Neither half is optional, and neither can be exercised further without code.
The bench has given all it can until `bus_lines()`, `listen_only()` and the
never-take-system-control path exist.

### 14.10 The 53310A captured — and it is PCL, not HP-GL

With the instrument in talk-only and the daemon issuing back-to-back `++read`,
pressing PRINT produced **4718 bytes in a single EOI-terminated read**, and the
front panel went to "printing." / "&lt;ANY KEY ABORTS PRINTING&gt;".

The content is not HP-GL. It is PCL raster graphics:

```
ESC * r 640 S     raster width, 640 pixels
ESC * r A         start raster graphics
ESC * b 74 W      transfer one row, 74 bytes   (x59)
```

61 escape sequences, 59 raster rows, and **no vector commands at all** — no
`IN;`, `PU`, `PD`, `PA`. The instrument prints a bitmap to a printer; it does
not plot to a plotter. That is the real reason it has no plotter-address
setting, and the reason its front panel says PRINT where the SR620 says plot.
The instrument also has a printer setup mode which requires the printer to be
in listen-only, corroborating §4.7 from the other side.

Kept as `contrib/fixtures/53310a-print.pcl`.

**Why it worked at all, and why it is incomplete.** A `++read` makes the
adapter an addressed listener and drops the RFD holdoff, so for the duration of
that read the instrument finally sees a ready listener. Between reads the
holdoff returns, the instrument stops finding one, and the remaining rows are
lost — hence one chunk and then nothing for the remaining 50 seconds. This is
§4.7 demonstrated in both directions in a single experiment.

So case 1 is *partially* reachable today by accident of the read path, and
`AUX_LON` is what turns an accidental partial capture into a continuous one.

### 14.11 "Setup printer" is send-only, so the 53310A needs no persona

Running the 53310A's SETUP PRINTER with the daemon reading produced exactly
eight bytes and nothing else for the remaining sixty seconds:

```
1b 45              ESC E          PCL printer reset
1b 26 6c 36 36 50  ESC & l 66 P   page length = 66 lines
```

It configures and stops. It does not ask the printer anything, and does not
wait for an answer.

That is a real simplification, and it separates this instrument sharply from a
plotter. An HP-GL plotter has to answer `OI`, `OP`, `OE` and friends (§13.1)
before some instruments will proceed, which is why §8 needs a persona at all.
The 53310A needs no persona, no address, and no ability to talk back. The
entire job for it is:

1. unaddressed listen (`AUX_LON`, §2), and
2. do not re-assert the RFD holdoff while capturing (§4.7).

Nothing in §8 is required for this instrument. The SR620 still needs all of it.

### 14.12 Listen-only works: the fix was the addressing

`UNL, MLA(us)` with **no talk address**. That is the whole fix.

```
44690 bytes, 558 PCL raster rows, ESC*rB end-of-raster present
two consecutive PRINT presses, both captured
the 53310A reported no error for the first time
```

Compare 307 rows and no end marker at the previous best, and zero bytes for
every attempt in between.

Three addressing sequences were tried in listen-only, and only the third
describes the bus correctly:

| Sequence | Result |
|---|---|
| no command bytes at all | nothing captured; the read engine is never armed |
| `UNL, MLA(us), MTA(0)` | nothing captured; designates a talker at address 0 that does not exist |
| `UNL, MLA(us)` | **works** — one listener, us; whoever drives the bus unaddressed is the talker |

The lesson is that `HR_LON` alone does not get the chip where it needs to be.
Reaching LACS through the ordinary addressing path is what arms the adapter's
read engine, and a talk-only source needs no talker designated because it is
already talking.

`0x31 NDAC NRFD REN` between reads was a red herring throughout. `nec7210_read`
(`nec7210.c:486-511`) holds off after *every* byte and releases it as each one
is consumed, so NRFD asserted between bytes is the handshake working. The
readiness that matters exists only inside an armed read, and is not observable
from another client because the backend lock serialises them.

§14.14 below is kept as written. Its withdrawal of the 307-row result still
stands — that capture was never explained, and the mechanism now working is not
the one it assumed.

### 14.13 SR620: case 3 confirmed, case 4 ruled out

Bus lines sampled continuously while PRINT was pressed, plotter mode on with
plotter address 5, nothing at address 5 on the bus:

```
291709 samples over ~70 s, every one: 0x39 NDAC NRFD EOI REN
```

Not one line moved. At roughly 4000 samples/second nothing transient could hide
in that, so this is a strong negative rather than a missed event.

- **No ATN.** The SR620 never attempts to take control, which **rules out case
  4**: no TCT handshake is needed on either side, and §8's pass-control work is
  not required for this instrument.
- **No DAV, no handshake movement.** It is not transmitting and failing — it is
  not transmitting at all.

Combined with §14.6 (zero bytes to a controller-side read, and `*ESR?` showing
no error bits), the picture is complete: the SR620 waits for a device to exist
at address 5 and does nothing whatsoever until one does. Listen-only does not
help, because there is no traffic to listen to.

**What it needs is the narrow version of §8**: be an addressable device at a
configured address. Not pass control, not a controller subset — just exist at
address 5 and answer when addressed. `--listen-address` plus the
never-take-system-control path from §1.

Whether it also queries the plotter (`OI`, `OP`, `OE`) and therefore needs a
persona is still unknown, and cannot be known until something answers at 5. The
53310A turned out to need none (§14.13); the SR620 has a plotter address, which
is weak evidence it expects a conversation.

### 14.14 SR620 plots, through device mode

With the adapter as a device at address 5 (`++dev 5`), PRINT produced **4408
bytes of HP-GL** — the first time the SR620 had emitted anything at all:

```
DF;SC-30,255,-20,205;TL2;SR.8,2;SP1;PU0,200;PD0,0,250,0,...
268 PA, 252 PD, 29 LB, 22 CP, 21 PR, 9 XT, 7 YT, 5 PU, 2 SP, 2 LT
ends: PU-30,-20;SP0;
```

Vectors, not raster — the opposite of the 53310A, sharing nothing but the
transport. Kept as `contrib/fixtures/sr620-plot.hpgl`.

**No output instructions.** No `OI`, `OP`, `OE`, `OS` anywhere in it. So the
SR620 needs no plotter persona either, and with both instruments send-only that
half of §8 has no customer on this bench and was not built. It could not have
been known earlier: an instrument that emits nothing until addressed cannot be
observed *not* asking questions.

`SP0` — pen away — is a natural end-of-plot marker, and exactly the kind of
format-level boundary §5 argues belongs in the client rather than the daemon.

Device mode itself was verified on the bus before the instrument was involved:
`0x01 REN` as controller, `0x00` with nothing asserted as a device, writes
refused, and clean recovery to controller afterwards with the SR620 answering
again.

### 14.15 The 82357B: right chip state, no bytes

Tried against the same 53310A, after the NI result. Three findings, in
increasing order of how much work they imply.

**The chip reaches a ready listener.** Entering listen-only took the adapter
from `0xa1 ATN NDAC REN` to `0x21 NDAC REN` — ATN released, NDAC asserted, and
**NRFD released**, which is a genuinely *ready* listener and more than the NI
ever showed. `AUX_LON | AUX_CS` (`tms9914.h:262`, with the `AUX_CS` set/clear
convention) is a true hardware listen-only here, where the TNT4882's `HR_LON` is
not enough on its own.

**ATN handling differs from the NI, and it matters.** The 82357 holds ATN
asserted at idle as CIC; only `AUX_GTS` inside a read releases it. So
re-addressing on every read asserts ATN on every re-arm, and ATN asserted
mid-transfer aborts the talker — the 53310A reported **"printing aborted"**
after transferring a large part of a page. The NI tolerates per-read addressing
(it is what was measured working there); the 82357 needs the addressing hoisted
into mode entry. **Do not unify these two paths without re-measuring both.**

**What actually blocks it: a bulk-pipe desync.** With the addressing moved into
`set_listen_only`, the capture stream delivered exactly one byte — `0xfb`, which
is not instrument data at all but the 82357's own response code for `WR_REGS`
(`!0x04`). The entry sequence interleaves register writes with command-byte
sends in a way that leaves a response in the bulk-in pipe, and the next GPIB
read consumes it as data.

This is the same class of fault as an unrelated nuisance seen all session:
killing the daemon mid-transfer leaves the adapter desynced, `init` fails with
`unexpected response byte 0xfa, expected 0xfb`, and a second start succeeds
because the stale bytes get drained. Both say the 82357 bulk protocol is
strictly request/response and this code does not always keep it that way.

**Retried after the desync fixes (still not working).** Two real bugs were
found and fixed in the attempt, and both were mine rather than the adapter's:

* The read path wrapped its bulk-in in `tokio::time::timeout`, which *drops*
  the in-flight transfer. A capture read times out on every quiet interval by
  design, so the pipe desynchronised within seconds. The stray `0xfb` in the
  capture stream was this adapter's own `WR_REGS` response arriving late. Fixed
  by resynchronising after an abandoned read.
* `init` did not drain a pipe left dirty by a dead predecessor, so a fresh
  daemon failed its first attempt and succeeded on the second. Now first time.

Also removed: an `UNL, MLA(0)` addressing step added to mode entry on the NI's
example. It is wrong here — `AUX_LON | AUX_CS` is a *true* hardware listen-only
on the TMS9914 where the TNT4882's `HR_LON` is not, so the addressing was
unnecessary, and worse, with only a talk-only instrument on the bus those
command bytes have no acceptor. The failure path calls `recover_from_stall`,
which pulses IFC.

After all three, the adapter reaches `0x21 NDAC REN` — ATN released, NRFD
released, a genuinely ready listener, better than the NI ever shows — the pipe
stays clean, and init succeeds first time. **And still no bytes arrive.** So
the remaining fault is in how the read engine is armed in this mode, not in the
chip state or the pipe hygiene, and it wants study rather than more attempts at
the bench.

**Four theories tried at the bench, none of them it.** Recorded so nobody
spends the presses again:

| Theory | Outcome |
|---|---|
| Entry needs `UNL, MLA(us)` like the NI | No. Worse — the failure path pulses IFC |
| Capture reads must not end on EOI | No, and backwards: without a termination flag the read can only end on count, times out, and the abort discards the page |
| `max_len` 65536 wraps a 16-bit count | No. The kernel encodes a full 32-bit LE length with no clamp, and our packet matches it byte for byte |
| The adapter must see `MLA(us)` go by to arm its read engine | No, even with the command sent and ATN dropped afterwards |

**Where to look next, and it is not the bench.** The adapter reaches a ready
listener, the pipe is clean, init is first-attempt, and the read command on the
wire is byte-identical to the kernel's. So the fault is in what the adapter
requires before it will collect bytes in this mode — and since the in-tree
drivers never use listen-only, there is no reference to compare against. A USB
capture of the adapter under a driver that does would settle it.

**Status: not working, and not a quick fix.** The chip reaches the right state;
something about how the transfer is set up does not. `set_listen_only` on this backend needs its register
writes and command sends ordered with the same care as `send_command_bytes`
already takes, and `init`/open should drain the bulk-in pipe before trusting it.

### 14.16 What was known before the fix

Honest status after the first implementation session, because the commit log
alone reads more positive than the result.

**What works:** `bus_lines()`/`++lines`; entering and leaving the mode (`++lon`,
`--listen-only`), verified over repeated cycles with ordinary traffic working
after each; the capture front-end's plumbing; and refusing writes while in the
mode.

**What does not:** the mode does not make the adapter a *ready* listener, which
is the entire point of it.

```
controller mode:  0x01 REN            not a listener at all
listen-only:      0x31 NDAC NRFD REN  a listener, but NRFD asserted
```

NRFD asserted is "not ready for data", and an HP 53310A in talk-only responds to
exactly that with **"no ready listeners?"**.

**A claim from §14.12 has to be withdrawn.** That section reported a 307-row
capture through polled `++read` and attributed the missing tail to gaps between
polls. Re-running the identical script later failed the same way as everything
else, so the 307-row capture was not a partial success of a working mechanism —
it was something transient that has not been reproduced or explained. The
continuous-read loop in `src/capture.rs` was built on that reading and is
therefore not yet justified by evidence, however sound its own reasoning is.

**What the reference says, which changes the diagnosis.** `nec7210_read`
(`nec7210.c:486-511`) shows this chip family holds off after *every byte*
(`HR_HLDA`) and the driver releases the holdoff explicitly after consuming each
one. So NRFD asserted between bytes is the handshake working, not a
misconfiguration, and the USB firmware runs that loop internally during a read.
An armed read therefore *should* present as ready — which moves suspicion off
the holdoff configuration and onto how the read is set up.

**Current hypothesis, implemented but untested against an instrument.** Two
addressing sequences have been tried in listen-only and both failed: sending no
command bytes at all, and sending `UNL, MLA(us), MTA(0)` — which designates a
talker at address 0 that does not exist. The third, now in the code, is
`UNL, MLA(us)` with *no* talk address: one listener, us, and whoever is driving
the bus unaddressed is the talker. That also reaches LACS through the normal
addressing path instead of relying on `HR_LON` alone, which matters if the
adapter firmware arms its read engine off the addressing.

**If that fails too**, stop guessing from the nec7210 header — the kernel never
sets `HR_LON`, so there is no reference for this state — and get a trace of the
53310A printing to a *real* printer. What a satisfied listener looks like on the
wire would settle it in one capture.

### 14.17 Still unmeasured

C2 (does talk-only monopolise the bus) needs two instruments attached at once,
one of them talking.

### 14.18 The adapter refuses reads it does not consider itself addressed for

Where §14.15 ended ("the fault is in what the adapter requires before it will
collect bytes in this mode — and it wants study rather than more attempts at
the bench"), studying the adapter delivered. What follows is the observed
behavioural contract.

**The adapter can refuse a read outright, and says so.** A read issued while
the adapter does not consider itself a listener is not armed at all: it
completes *immediately*, with zero data bytes and a trailing flags byte of
`ATRF_UNADDRESSED` (`0x80`). This is a different observable from an armed
read on a quiet bus, which runs until count, terminator, or host timeout.

**Whether it considers itself a listener follows the addressing it transmits.**
The adapter tracks the command bytes it is asked to send: `MLA(us)` marks it a
listener, `UNL` un-marks it, `MTA(us)` / `UNT` do the same for talking — the
same job as `check_my_address_state()` in the kernel's `tms9914.c`, done
inside the adapter. `AUX_LON | AUX_CS` raises the same listener state
standing. This is why ordinary controller reads always worked: their
`UNL, MLA(0), MTA(pad)` preamble re-marks the adapter a listener every time.
And it is why order in `set_listen_only` was a landmine: sending `UNL` *after*
raising listen-only silently un-marks the adapter, and every capture read
thereafter is refused. The addressing now goes first, the mode bit last.
Command bytes that fail to handshake (no acceptor on the bus) do not count —
which retroactively invalidates one §14.15 theory-table entry: "the adapter
must see `MLA(us)` go by — tried, didn't help" was never actually tested,
because that MLA never completed its handshake.

The listener state is also lost on adapter reset and USB suspend/resume.
Notably *not* on the list: `XFER_ABORT`, so the capture loop's timeout path
is innocent.

**A refused read and a quiet bus were indistinguishable — now they are not.**
`decode_gpib_read_response` used to discard the trailing flags byte unless it
was EOI/EOS, so a capture stream fed by refused reads (instant, empty,
`ATRF_UNADDRESSED`) looked identical to one fed by armed reads on a silent
bus (timeout, empty). Every §14.15 bench session ran blind to this
distinction. The backend now logs the trailing byte and the read duration,
and warns specifically on `ATRF_UNADDRESSED`. Next bench session, that one
log line picks the branch: refusals mean the listener state got dropped
(the causes are enumerated above); genuine timeouts while an instrument is
talking mean the fault is below the protocol layer, and the USB capture of a
working driver that §14.15 wanted becomes the right tool — with the narrower
question "what does an armed read look like".

**Verified on hardware** (82357B, empty bus): with listen-only raised, a
capture read *arms* — it ran the full host timeout on the silent bus. With
the listener state dropped, the identical read returned in 0 ms with zero
bytes and trailing `ATRF_UNADDRESSED`, and the new warn line fired. The
refusal-vs-quiet-bus discriminator works; what still needs an instrument is
whether the armed read then collects (`examples/lon_gate_probe.rs` is the
probe). One incidental find, fixed in the same change: after a command send
that fails for lack of an acceptor, the adapter answers nothing further on
the bulk pipe until `XFER_ABORT` — and the failure can look like success from
the host — so `set_listen_only` now aborts and drains unconditionally after
its self-addressing attempt. Mode entry on an empty bus leaves the lines at
`ATN | REN`; that is the §14.15 hold-ATN-at-idle behaviour, and ATN release
belongs to the armed read, not to mode entry.

### 14.19 Resolved: 82357B listen-only capture works

An HP 53310A print captured complete through an 82357B: 30,745 bytes, byte
level identical in format to the NI fixture — same PCL preamble
(`\e*r640S\e*rA\e*b74W…`), full raster body, proper `\e*rB` terminator. Two
further faults beyond §14.18's gate, each of which masked the next, and each
invisible without the previous one fixed:

**The chip free-ran the acceptor handshake.** With listen-only raised and no
RFD holdoff configured, the TMS9914 completes the handshake for every byte on
its own; the data is gone before the collection engine can take it. The
talker finishes its entire transmission convinced it had a listener — it did:
the chip, acknowledging bytes into the void. Init clears holdoff mode (the
kernel init sweep it mirrors does the same, correctly for a *controller*),
and mode entry's `AUX_VAL` release — added to fix §14.16's "no ready
listeners" — completed the free-run. The fix is `AUX_HLDA | AUX_CS`
(holdoff on all) at mode entry, cleared on exit, exactly the discipline
`nec7210_read` uses per byte. Confirmed at the bench in isolation: with
holdoff set and *no* read armed, a print accepts one byte and then holds —
NRFD stays asserted and the talker waits — where before it "printed
successfully" into nothing.

**The timeout path discarded everything an armed read had collected.**
`tokio::time::timeout` *drops* the in-flight bulk transfer, and with it every
byte buffered inside. A capture read times out on every quiet interval by
design, and a print smaller than the 64 KiB request that does not end in EOI
never completes the read on its own — so the engine collected entire pages at
full speed and the host threw them away, indistinguishable from "no bytes
ever arrived". §14.15's theory table even contains the mechanism ("the abort
discards the page") — it was rejected only because `end_on_eoi` was assumed
to terminate reads early, and this instrument does not assert EOI mid-page.
The fix: on timeout in listen-only, do not abandon the transfer — send
`XFER_ABORT` while the bulk read is still pending; the adapter ends the
transfer with a trailing flags byte (`ATRF_ABORT`), the pending read
completes with the data, and the salvaged bytes go to the client. A quiet
interval now returns an empty successful read rather than an error.

With all three fixed, the capture loop runs: armed reads salvage on every
timeout, holdoff parks the talker across re-arm gaps (NRFD held, no bytes
lost in the gap), and the §14.18 logging distinguishes refusal, quiet, and
collection at a glance.

Found and not yet fixed, noted for later: the capture front-end's
client-departure watcher did not fire when a client was killed mid-session —
the loop kept arming reads until the mode was toggled off (`src/capture.rs`,
the `watcher` select arm).
