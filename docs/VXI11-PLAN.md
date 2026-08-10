# VXI-11 front-end: plan and spec

Status: **plan** — nothing here is implemented yet. This document is the
blueprint for building it, sized so each phase can be handed to an agent (or a
human) with the spec references and acceptance criteria it needs. Phases land
as sequential PRs, each merged before the next starts — never stacked.

## Why

The HiSLIP front-end must *guess* whether a command produces output, because
HiSLIP has no read request: the server pushes replies. The current heuristic
(quote-aware `?` hint, then one serial poll for MAV — commit `ac6e5dd`) is as
good as that guess can get, and it is still structurally blind to a whole
instrument class: pre-488.2 devices that only produce data when addressed to
talk. An HP 8594E's `PRINT` screen dump has no `?`, sets no MAV (the 8590
series status byte predates 488.2 and has no MAV bit), and generates its PCL
output *on demand when addressed*. No signal the daemon can observe says
"read now" — the information exists only in the client, which is blocked in
`viRead` sending nothing.

VXI-11 carries that information on the wire. `device_read` is an explicit RPC:
the client's `viRead` becomes a message, the daemon addresses the instrument to
talk in direct response, and returns what it says. This is not a workaround —
VXI-11 was designed as the LAN-to-GPIB gateway protocol (it is what the
HP/Agilent E2050 and E5810 speak), and its semantics match GPIB exactly. The
heuristic remains, but only where the protocol forces it (HiSLIP); VXI-11
clients get the real thing.

Verified before writing this plan: the 8594E streams 18,425 bytes of PCL
through ugpibd's client-driven Prologix front-end (`PRINT` then `++read eoi`)
on the first try, same daemon, same NI GPIB-USB-HS. Only the read *decision*
is missing, not any bus capability.

## What we are building

- A complete VXI-11 server front-end: core channel, abort channel, interrupt
  (SRQ) channel, locking — no stubs. Quality bar is the HiSLIP front-end:
  spec-conformance tests keyed to section numbers, integration tests over real
  TCP, honest errors ("no plausible lies", see ROADMAP guiding rule).
- A VXI-11 client codec (mirroring `hislip/client.rs`) used by the test suite
  and the CLI.
- `ugpibd-scpi` grows transport selection: HiSLIP (default, unchanged),
  VXI-11, and Prologix.
- An **optional** embedded portmapper for commercial-VISA compatibility, off
  by default. pyvisa-py needs none of it (`TCPIP::host,port::gpib0,18::INSTR`
  bypasses portmap — `Vxi11CoreClient` documents this as the supported way to
  tunnel/firewall VXI-11).
- HiSLIP must not regress: its test suite stays green through every phase, and
  the shared-plumbing refactor (Phase 1) is behavior-preserving by
  construction.

## Spec reading list

Agents implementing a phase should read the relevant sections before coding;
each phase below cites its sections.

| Document | What it covers | Where |
|----------|----------------|-------|
| VXI-11 (rev 1.0, 1995-07-17, VXIbus Consortium) | The protocol: channels, links, all RPC procedures, error codes, flags | vxibus.org spec archive (also mirrored widely; pyvisa-py's `protocols/vxi11.py` transcribes the constants) |
| VXI-11.2 | GPIB (488.1) gateway mapping: `gpib0,pad[,sad]` device names, interface devices, `device_docmd` bus operations | same |
| VXI-11.3 | 488.2 instrument rules (what a *device* behind the gateway is expected to do; informs defaults, not code) | same |
| RFC 5531 (obsoletes 1057) | ONC RPC v2: call/reply framing, auth flavors, accept/reject codes | ietf.org |
| RFC 4506 | XDR encoding | ietf.org |
| RFC 1833 | Portmap v2 / rpcbind (only §3, the v2 portmap program) | ietf.org |

Ground truth for interop is pyvisa-py's `pyvisa_py/protocols/vxi11.py` +
`rpc.py` (client side of everything we serve) — local checkout at
`~/code/fygm/pyvisa-py/`.

## Protocol constants (transcribed for convenience)

Programs:

| Program | Number | Version | Who serves it |
|---------|--------|---------|---------------|
| DEVICE_CORE | 0x0607AF (395183) | 1 | ugpibd (core channel, TCP) |
| DEVICE_ASYNC | 0x0607B0 (395184) | 1 | ugpibd (abort channel, TCP) |
| DEVICE_INTR | 0x0607B1 (395185) | 1 | **client** (we call it back for SRQ) |

DEVICE_CORE procedures: `create_link`=10, `device_write`=11, `device_read`=12,
`device_readstb`=13, `device_trigger`=14, `device_clear`=15,
`device_remote`=16, `device_local`=17, `device_lock`=18, `device_unlock`=19,
`device_enable_srq`=20, `device_docmd`=22, `destroy_link`=23,
`create_intr_chan`=25, `destroy_intr_chan`=26.
DEVICE_ASYNC: `device_abort`=1. DEVICE_INTR: `device_intr_srq`=30.

Error codes (`Device_ErrorCode`): 0 no error, 1 syntax error, 3 device not
accessible, 4 invalid link identifier, 5 parameter error, 6 channel not
established, 8 operation not supported, 9 out of resources, 11 device locked
by another link, 12 no lock held by this link, 15 I/O timeout, 17 I/O error,
21 invalid address, 23 abort, 29 channel already established.

Flags (`Device_Flags`): bit 0 `waitlock`, bit 3 `end` (device_write: assert
EOI on last byte), bit 7 `termchrset` (device_read: `termChar` is valid).

`device_read` reason bits: 0x1 `REQCNT` (requestSize satisfied), 0x2 `CHR`
(termchar hit), 0x4 `END` (EOI). A read that fills `requestSize` without END
sets REQCNT and the client comes back for more — chunking falls out naturally.

## Architecture

```
src/vxi11/
  mod.rs        constants, STANDARD-less: no IANA port; our default --vxi11-port
  xdr.rs        XDR primitives: (u)int, string/opaque (variable+fixed), arrays
  rpc.rs        ONC-RPC: record marking (fragmentation!), call/reply headers,
                auth (AUTH_NONE accepted, others ignored per RFC), accept/deny
  messages.rs   VXI-11 XDR structs: Create_LinkParms/Resp, Device_WriteParms,
                Device_ReadParms/Resp, Device_GenericParms, Device_LockParms,
                Device_DocmdParms/Resp, Device_EnableSrqParms,
                Device_RemoteFunc, Device_SrqParms, Device_Error
  server.rs     core-channel server: link table, per-link state, dispatch
  abort.rs      DEVICE_ASYNC listener + in-flight-operation cancellation
  intr.rs       interrupt channel: RPC *client* back to the VISA client
  client.rs     VXI-11 client (CLI + tests), mirroring hislip/client.rs
  portmap.rs    optional portmap v2 responder (UDP+TCP), GETPORT/NULL/DUMP
src/frontend/   (new, extracted in Phase 1)
  lock.rs       the lock table, moved from hislip/lock.rs, shared by both
  instrument.rs shared GPIB instrument ops with SPLIT write/read
```

### The split-write/read abstraction (Phase 1, the load-bearing refactor)

HiSLIP's `Device::execute(cmd, expect_response)` *fuses* write and read,
because HiSLIP forces the read decision to write time. That fusion is exactly
what VXI-11 exists to avoid, so VXI-11 must not sit on `Device`. Plan:

- Extract a `frontend::Instrument` (name TBD by implementer) owning the
  `SharedBackend` + pad, exposing the *split* primitives: `write(cmd, eoi)`,
  `read(max, termchr: Option<u8>)`, `serial_poll`, `trigger`, `clear`,
  `remote/local/lockout`, `srq` subscription — a thin, honest mapping of
  `GpibBackend` scoped to one pad, with the same `resource_key()` identity
  ("gpib{pad}") used today.
- `hislip::GpibInstrument` reimplements `Device::execute` *on top of* the
  split primitives — same poll-MAV-after-write logic, same SRQ windows,
  verbatim. HiSLIP behavior does not change; its tests prove it.
- The lock table moves from `hislip/lock.rs` to `frontend/lock.rs` unchanged.
  Cross-protocol coherence is a feature, not an accident: a `viLock` taken
  over HiSLIP and one taken over VXI-11 on the same pad must exclude each
  other, because it is the same instrument. Keyed by `resource_key()` as now.

### Sub-addressing / device names

Per VXI-11.2: `gpib0,18` (pad), `gpib0,18,96` (pad+sad — parse and reject
with error 21 until secondary addressing exists on the backend, honestly, not
silently), and the bare interface device `gpib0` (Phase 6). Also accept
`inst0` and `gpib0` as "daemon default PAD" for symmetry with the HiSLIP
sub-address rules (`--default-address`). Note the asymmetry with HiSLIP is
deliberate: HiSLIP avoids commas because pyvisa-py parses `hislip0,15` as a
port; for VXI-11 device names, commas are the spec's own syntax and pyvisa-py
handles them correctly.

### Timeouts

`device_read`/`device_write`/etc. carry per-call `io_timeout` (ms) and
`lock_timeout`. These are honored per operation — the backend gets
`set_timeout(io_timeout)` for the call and restored after — rather than
mapped onto the daemon-global `--timeout-ms`, which becomes only the default
for calls that pass 0. This is a semantic improvement over HiSLIP (which has
no per-op timeout) and is what makes visashot's 25 s model-specific timeout
actually reach the bus.

### Abort (real, not a stub)

`device_abort` on the async channel names a link; any in-flight core-channel
operation on that link must terminate promptly with error 23. Implementation:
each link's in-flight operation registers a cancellation handle (tokio
`select!` on a per-link `Notify`); the bus transaction itself is bounded by
`io_timeout`, so cancellation takes effect at the next await point, and the
backend is left addressed-idle (send UNT/UNL — via the existing backend ops —
before releasing the bus mutex). Conformance test: a slow `TestBackend` read
aborted mid-flight answers 23 and the *next* operation on the link succeeds.

### SRQ / interrupt channel

`create_intr_chan(hostAddr, hostPort, progNum, progVers, progFamily)` — we
connect *back* to the client (TCP; UDP family answers error 8 operation not
supported, honestly) and hold the connection. `device_enable_srq(enable,
handle)` stores the opaque handle (≤40 bytes) per link. On the backend's SRQ
broadcast (same `subscribe_srq` the HiSLIP session uses), serial-poll to
confirm RQS as the HiSLIP path does, then fire `device_intr_srq(handle)` as a
one-way call to every enabled link's channel. pyvisa-py's
`SrqInterruptTCPServer` is the interop target; the visashot idiom is not
affected but `contrib/multidev_srq.py` gains a VXI-11 mode to prove parity
with the HiSLIP SRQ semantics on real hardware.

The HiSLIP front-end's RQS-consumption caveat (documented in HISLIP.md)
applies identically: whoever serial-polls eats RQS. The shared plumbing must
keep today's behavior of surfacing a consumed status byte to the session that
will report it.

### Locking

`device_lock`/`device_unlock` map onto the shared lock table. Differences
from HiSLIP to encode: VXI-11 *does* have a "locked" error (11) — with
`waitlock` clear, a conflicting request fails immediately with 11; with
`waitlock` set, it waits up to `lock_timeout` then fails 11. All VXI-11 locks
are exclusive (there is no shared-lock string as in HiSLIP); a HiSLIP shared
lock held on the same instrument counts as a conflict. Locks release on
`destroy_link` and on channel teardown (client crash), as HiSLIP does on
session close.

### device_docmd (VXI-11.2 interface operations, Phase 6)

Served on interface-device links (`gpib0`): cmd 0x020000 send command bytes
(ATN-addressed data — maps to backend command writes), 0x020001 bus status
(map from `BusLines` + controller state: REN, SRQ, NDAC, ATN, plus
system-controller/talker/listener sub-queries), 0x020002 ATN control,
0x020003 REN control, 0x020004 pass control (error 8 — we do not pass
control; honest refusal), 0x02000A bus address set. Device links (`gpib0,N`)
answer docmd with error 8. Anything unimplementable by a backend surfaces as
error 8, never a fabricated success — ROADMAP guiding rule.

### Portmapper (optional, Phase 7)

`--portmap` off by default. When on: portmap v2 (RFC 1833 §3) on UDP and TCP,
answering NULL, GETPORT (DEVICE_CORE and DEVICE_ASYNC → their bound ports),
and DUMP (so `rpcinfo -p` works); everything else PROC_UNAVAIL. Port 111 is
privileged: the deb ships a systemd socket unit (`ugpibd-portmap.socket`)
so systemd binds 111 and passes the fds — the daemon itself never needs
CAP_NET_BIND_SERVICE; `--portmap-port` exists for unprivileged testing.
Startup refuses loudly (with a pointer to rpcbind) if 111 is already owned.
This phase exists purely for NI/Keysight VISA stacks that hardwire portmap
lookup; pyvisa-py documentation in VXI11.md leads with the fixed-port string.

### CLI (`ugpibd-scpi`, Phase 8)

`--transport hislip|vxi11|prologix` (default hislip — no behavior change),
with `--port` defaulting per transport (4880 / `--vxi11-port` default / 1234).
The REPL loop is transport-agnostic: a small `Transport` trait (write, query,
read, readstb, trigger, clear, lock, local/remote) implemented by
`HislipClient`, the new `vxi11::client`, and a Prologix line client (which the
`++`-meta commands map onto almost 1:1). `query_hint` stays what decides
write-vs-query for hislip and prologix; for VXI-11 it merely decides whether
the REPL *issues* a `device_read` after the write — a wrong hint is now a
user-visible non-answer rather than a protocol corruption, and `++read` in
the REPL forces one, which is the manual escape hatch the 8594E class needs
even at the CLI.

## Testing strategy

Same three layers as HiSLIP, plus interop:

1. **Unit** (in-module): XDR roundtrips including edge cases (padding,
   max-length opaques, truncation errors); RPC record marking with
   multi-fragment records both directions; portmap encode/decode; every
   `Device_ErrorCode` mapping. Property-style roundtrip tests where cheap.
2. **Integration** (`tests/vxi11_integration.rs`): real TCP against the
   server with a `TestBackend`/`TestDevice`-style mock (reuse the Phase-1
   split abstraction so the mock implements the split primitives), driven by
   `vxi11::client`. Conformance tests named for and citing spec sections,
   in the house style (`tests/hislip_integration.rs` is the model):
   link lifecycle, maxRecvSize enforcement (error 5 on oversized write),
   read chunking reason bits, termchr reads, per-call io_timeout (15),
   waitlock/lock_timeout (11), abort (23), SRQ delivery with handle echo,
   invalid link (4), docmd on device links (8), destroy with in-flight op.
3. **Cross-front-end**: one test that takes a HiSLIP lock and proves a VXI-11
   `device_lock` waits/fails per waitlock, and vice versa — the coherence
   property is load-bearing and must not silently split into two tables.
4. **Interop** (`contrib/`): a pyvisa-py smoke script (like
   `hardware_exercise.py`) with `--transport vxi11`, run against real
   hardware; the E2050/E5810 wire behavior is the reference where the spec is
   ambiguous. Hardware checklist (HARDWARE-TEST.md) grows a VXI-11 section:
   two instruments on the bus (addressing isolation), 34401A SRQ idiom,
   53132A, and the acceptance test for this whole effort —
   **visashot capturing the 8594E screen** with stock pyvisa-py via
   `TCPIP::<host>,<port>::gpib0,18::INSTR`, no visashot changes.

**HiSLIP regression guardrail:** every phase runs the full existing suite;
Phase 1's PR description must show the hislip integration tests unchanged
(not adapted — unchanged) and green. Any hislip test edit in later phases is
a red flag to review, not a routine diff.

## Phases / PRs

Sequential; each PR is self-contained with its tests and docs, merged before
the next begins.

| PR | Contents | Acceptance |
|----|----------|------------|
| 1 | Extract `frontend/` (split-primitive instrument + lock table); hislip reimplemented on top | Zero hislip test changes, all green; no new behavior |
| 2 | `xdr.rs` + `rpc.rs` + `messages.rs` (+ portmap codecs, unused yet) | Unit suite incl. fragmentation; no daemon wiring |
| 3 | Core-channel server + `client.rs`; `--vxi11-port` (pick default, suggest 9010; 0 disables); link table; write/read/readstb/trigger/clear/remote/local/destroy; per-call io_timeout; termchr; maxRecvSize | Integration suite; pyvisa-py opens `TCPIP::127.0.0.1,9010::gpib0,18::INSTR` and does write/query/read_stb against a bench instrument |
| 4 | Locking + abort channel + cancellation | Lock conformance incl. cross-front-end coherence; abort test; error 11/12/23 paths |
| 5 | Interrupt channel + `device_enable_srq` | SRQ conformance tests; `contrib/multidev_srq.py --transport vxi11` passes on hardware |
| 6 | VXI-11.2 interface device: `gpib0` links, `device_docmd` set above; sad parsing (honest 21) | docmd conformance tests; bus-status maps `BusLines` |
| 7 | Optional portmapper + deb systemd socket unit + rpcbind coexistence | `rpcinfo -p` works against it; NI-VISA (or Keysight IO Libs, whichever is on the bench Mac/VM) discovers and connects |
| 8 | CLI transports (vxi11, prologix) via `Transport` trait | `tests/scpi_cli.rs` extended; manual REPL against 34401A on all three transports |
| 9 | Docs + hardware validation sweep: `docs/VXI11.md` (conformance notes in HISLIP.md style), HARDWARE-TEST.md section, README, CHANGELOG; run full checklist incl. 8594E/visashot end-to-end | The screenshot. Also: HiSLIP + Prologix checklist rerun to close the regression loop |

Suggested but not mandatory merges: 4+5 can combine if the abort plumbing
lands cleanly; 9 folds into 8 if the sweep is clean on the first pass.

## Open questions (decide during Phase 3 review, none block Phase 1–2)

- Default `--vxi11-port`: 9010 (suggested; memorable, unprivileged, unclaimed)
  — or serve VXI-11 on-by-default like HiSLIP vs. opt-in like Prologix.
  Leaning on-by-default: it is the front-end that needs no per-instrument
  caveats.
- Whether `device_read` with the instrument silent should map the backend's
  empty-read-no-END result to error 15 (I/O timeout) — almost certainly yes,
  and it is the honest mapping; noting it because HiSLIP's "no data and no
  END" bail becomes a *normal* code path here, not an anomaly.
- Secondary addressing (`gpib0,pad,sad`): error 21 now; whether to plumb sad
  through `GpibBackend` is its own roadmap entry, out of scope here.
