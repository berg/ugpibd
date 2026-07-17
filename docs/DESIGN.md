# `ugpibd` — a userspace Rust driver for the Agilent/Keysight 82357B, exposing a Prologix-compatible TCP server

**Status:** design doc, v0.1
**Scope:** handoff to a coding agent for an initial implementation.
**Target platforms:** Linux (primary), macOS (secondary). Windows is not a goal but shouldn't be foreclosed.
**License:** GPL-3.0-or-later.

---

## 1. Goals and non-goals

### 1.1 Goals

1. Talk to an **Agilent/Keysight 82357B** USB-to-GPIB adapter (USB ID `0957:0518` → `0957:0718` after firmware upload) from pure userspace — no kernel module, no `/dev/gpib*` device nodes, no `linux-gpib` runtime dependency.
2. Expose a **Prologix GPIB-USB-compatible TCP server** on a configurable port (default `1234`) so that existing client libraries (PyVISA with `prologix-gpib-async`, `plx-gpib-ethernet`, `pyprologix`, or raw socket code) can talk to it unchanged.
3. Run unmodified on modern Linux (glibc, Ubuntu 24.04+, arbitrary kernel version, including kernels without `CONFIG_GPIB_*`) and on macOS 12+ (Intel and Apple Silicon).
4. Ship the firmware-upload step inside the daemon — the user should not need `fxload` or udev rules to load firmware.
5. Be honest about scope: this is a hobby-grade daemon for well-behaved SCPI instruments on a single bus. It does not need to be a complete IEEE-488 stack.

### 1.2 Non-goals (v1)

- The 82357A (firmware file differs, USB 1.1, minor protocol deltas). Leave hooks but do not implement.
- Any other GPIB hardware.
- VXI-11 / HiSLIP / raw-socket SCPI-over-LXI — deferred; design should not preclude adding VXI-11 later.
- SRQ / serial poll / service requests — deferred. Polling-based SCPI works fine without them.
- Secondary GPIB addressing. The 82357A/B firmware doesn't support it anyway (see §4.4).
- Running in IEEE-488 "device" (non-controller) mode. The firmware doesn't support it anyway.
- Multiple simultaneous TCP clients against one adapter. Single-client, single-adapter.
- Hot-plug recovery beyond "daemon exits cleanly, systemd restarts it."

### 1.3 Explicit stretch items (think about these while structuring the code)

- Multiple adapters on one host, each on its own TCP port — structure so the USB device and the TCP listener are a single unit that can be instantiated N times.
- A future VXI-11 frontend sharing the GPIB-state-machine layer.
- A future SRQ path — leave the interrupt-IN endpoint wired up even if we don't expose it yet.

---

## 2. Licensing

**GPL-3.0-or-later for all project source.**

Rationale: the authoritative documentation for the 82357B wire protocol is the in-kernel `agilent_82357a` driver, which is GPL-2-or-later (and the out-of-tree `linux-gpib` project is GPL-3). Reading that code as a reference and re-expressing the protocol in Rust creates a derivative work. GPL-3 is compatible and matches the upstream project's spirit. Every source file gets an SPDX header:

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 <name>
```

The firmware blob (`measat_releaseX1.8.hex`) is redistributable per the `fmhess/linux_gpib_firmware` repo's stated terms. We include it as a build-time asset and embed it in the binary via `include_bytes!`.

---

## 3. Authoritative reference sources (read these first)

A coding agent should skim these before writing any code. In priority order:

| What | Where | Why |
|------|-------|-----|
| **Main kernel driver** | `drivers/staging/gpib/agilent_82357a/agilent_82357a.c` (kernels 6.13–6.18) or `drivers/gpib/agilent_82357a/agilent_82357a.c` (6.19+). Browse at <https://elixir.bootlin.com/linux/latest/source/drivers/staging/gpib/agilent_82357a> | Protocol-of-truth. All register opcodes, packet framing, timeouts, retry logic. |
| **Header** | `agilent_82357a.h` in the same directory | Opcode constants (`DATA_PIPE_CMD_*`, `XFER_*`, status bits). Copy these into a Rust `mod protocol`. |
| **Common GPIB layer** | `drivers/staging/gpib/common/gpib_os.c` and `include/ibconfig.h`, `include/gpib_ioctl.h` | Defines high-level ops (`ibwrt`, `ibrd`, `ibclr`, `ibtmo`). Useful for understanding what the hardware needs to be told to do for each client operation. |
| **Firmware upload reference** | `fxload` source (<https://github.com/tormodvolden/fxload> or the older `linux-hotplug` tarball). Specifically the `ezusb.c` Intel-HEX parser and the 8051-reset vendor-request logic. | We reimplement this in Rust; fxload is the cleanest reference. |
| **Firmware blobs + README** | <https://github.com/fmhess/linux_gpib_firmware> — specifically `agilent_82357a/measat_releaseX1.8.hex` and the `README` there. | Contains the critical note about the double-upload bug on the 82357B (see §5.3). |
| **Prologix protocol spec** | "Prologix GPIB-USB Controller User Manual" (search "prologix gpib-usb manual pdf"). Pay attention to the `++` command set. | Our TCP server exposes this dialect. |
| **Cypress FX2LP TRM** | "EZ-USB Technical Reference Manual" (AN-15456 / TRM-001). | Canonical source for the vendor request `0xA0` and the `CPUCS` register at `0xE600` used to hold/release the 8051. |

**Tip for the agent:** when you need to know what bytes go where, the kernel `agilent_82357a.c` is short (under 2000 LOC) and directly readable. Don't try to deduce the protocol from captures; just translate the C.

---

## 4. Hardware background

### 4.1 Physical

The 82357B is a captive-USB-cable dongle with a standard 24-pin IEEE-488 connector at the other end. Internally it's a Cypress **FX2LP** (CY7C68013A) microcontroller with a GPIB transceiver. It draws USB bus power; no external supply.

Three LEDs on the case: FAIL (red), READY (green), ACCESS (yellow). State on a correctly-initialized idle bus: only READY lit.

### 4.2 Two-phase USB enumeration

The FX2 always boots with no firmware. On initial plug-in the device shows up with:

- `idVendor = 0x0957` (Agilent)
- `idProduct = 0x0518` (uninitialized 82357A/B)

Only the FAIL LED is lit. Configuration space is minimal — one config, one interface, no bulk endpoints for GPIB data (just `EP0` for control).

After we upload firmware (§5), the device **renumerates**: it disconnects from USB and re-attaches, now advertising:

- `idVendor = 0x0957`
- `idProduct = 0x0718` (firmware-loaded 82357B)

This version exposes the endpoints we actually use. Inspect them at runtime — do not hardcode endpoint numbers; read them from the interface descriptor. The kernel driver does the same.

### 4.3 Endpoints (post-firmware)

The 82357B exposes roughly (read the `.h` file for exact names):

- **Bulk OUT** — host → device command/data packets ("pipe 2" in kernel lingo).
- **Bulk IN** — device → host response packets.
- **Interrupt IN** — asynchronous notifications (SRQ, status). We ignore this in v1 but must still claim it so it doesn't backlog.

Endpoint numbers and max packet sizes vary slightly between 82357A and B — do not hardcode.

### 4.4 Firmware-imposed limitations (copy into user-facing docs)

From the `linux-gpib` "Supported Hardware" page, the 82357A/B firmware cannot:

- Operate in **device mode** (non-controller). Must be system controller.
- Use a **secondary address**.
- Do **7-bit EOS comparison** (always compares 8 bits).

These are hardware/firmware limits, not things our driver could fix. Surface them as errors if a client requests them.

---

## 5. Firmware upload (`mod firmware`)

### 5.1 What it does

The FX2 has a "default USB device" mode. To make it behave as a 82357B, we must:

1. Parse the Intel-HEX firmware file (`measat_releaseX1.8.hex`, ~50 KB of 8051 code + data).
2. Issue vendor-request writes to load each HEX record into FX2 RAM.
3. Toggle the 8051 reset via the `CPUCS` register.

Details:

- **Vendor request:** `bmRequestType = 0x40` (host→device, vendor, device), `bRequest = 0xA0` (`ANCHOR_LOAD_INTERNAL`), `wValue = <target RAM address>`, `wIndex = 0`, `data = <record bytes>`.
- **CPUCS register** lives at FX2 internal address `0xE600`. Write `0x01` to **hold** the 8051 in reset. Write `0x00` to **release**.
- Upload sequence: hold reset → write all HEX records → release reset.
- After release, the device disconnects within ~100 ms and reappears (renumerates) with PID `0x0718`.

### 5.2 Intel HEX parsing

Write this from scratch; it's 50 lines. Each line: `:LLAAAATT[DD...]CC` where `LL`=byte count, `AAAA`=address, `TT`=record type (`00`=data, `01`=EOF, others rare and ignorable for this firmware), `DD`=data bytes, `CC`=checksum. Validate the checksum. Sanity-check total size (<64 KB).

### 5.3 **The double-upload bug** (critical)

Quoting the `fmhess/linux_gpib_firmware` README, there is apparently a bug in the 82357B firmware which causes the first firmware upload to an 82357B to fail. After the first attempt, the device will disconnect then reconnect (with a new address), but will still be in an uninitialized state with device id 518. Loading the firmware a second time will cause the device to disconnect again, but this time it when it comes back it will be fully functional with device id 718.

Implication for our code: after the first upload, wait for renumeration, re-open the device, check PID. If still `0x0518`, upload again. If after two attempts it hasn't come up as `0x0718`, bail with a clear error. This is specific to the B; the A doesn't need it but the logic is harmless to always apply.

### 5.4 Skipping upload when already loaded

If on startup we find a device with PID `0x0718`, skip the whole firmware dance and go straight to claiming interfaces. This matters because if the daemon is restarted without the adapter being unplugged, the firmware is still resident.

---

## 6. Wire protocol over bulk endpoints

**Do not try to design this.** Translate it from `agilent_82357a.c` / `.h`.

Approximate shape (verify against source):

- All host→device traffic on bulk OUT is **framed packets** with a 1-byte command byte followed by a payload.
- Command byte families (names from the header):
  - `DATA_PIPE_CMD_WRITE` (0x01-ish, I'm not sure of the exact value — check the header): write data bytes to the GPIB bus with EOI handling.
  - `DATA_PIPE_CMD_READ` (0x03-ish): initiate a read from the bus.
  - `DATA_PIPE_CMD_WR_REG` (0x04): write a TMS9914-style register.
  - `DATA_PIPE_CMD_RD_REG` (0x05): read a TMS9914-style register.
  - `DATA_PIPE_CMD_ABORT` — cancel an in-flight op.
- The payload structure for read/write includes a header (length, flags, primary address) followed by the data.
- Responses on bulk IN have a matching framing with status bytes (completion flags, error codes).

**Concretely:** implement `mod protocol` containing a `Packet` enum with variants mirroring the kernel's command byte constants, plus `encode()` / `decode()` functions. Unit-test round-tripping before touching real hardware.

### 6.1 "Registers" are a TMS9914 abstraction

The FX2 firmware exposes the 82357 hardware as if it were a TMS9914 GPIB controller chip. Most of what `gpib_os.c` does is massage TMS9914 register writes. You don't need to master the TMS9914 datasheet — just translate what the kernel driver does for each high-level op.

Register ops we need for v1:
- Set **primary address** (our own; we're always the controller at PAD 0).
- Set the current **talk/listen target** (the instrument's primary address).
- Assert / deassert **REN** (Remote Enable).
- Send **IFC** (Interface Clear) during init.
- Set / query **EOI** behavior on send.
- Set / query **EOS** (end-of-string) character.
- Issue **GPIB "device clear"** (selected or universal).

### 6.2 Timeouts

GPIB has 18 standard timeout values from `T10us` to `T1000s`. The kernel maps these to TMS9914 settings. For v1, expose a single configurable timeout (default 3 s) that gets mapped to the nearest GPIB standard value. Expose `++read_tmo_ms N` in the Prologix frontend.

### 6.3 EOI and EOS

Two independent "end of message" signals on GPIB:
- **EOI**: a physical line asserted with the last byte. Default on for writes in SCPI land.
- **EOS**: a character (typically `\n`) that the controller watches for on reads.

Prologix defaults (which we should match):
- Writes append `\n` and assert EOI on the last byte.
- Reads terminate on EOI OR on an EOS byte (default `\n`).

---

## 7. Prologix TCP protocol (`mod prologix`)

### 7.1 Framing

Line-oriented ASCII over TCP. Line terminator: `\n` (accept `\r\n`, emit `\n`). Lines starting with `++` are **controller commands**; everything else is **SCPI data** to forward to the currently-addressed instrument.

### 7.2 Commands we implement (v1)

| Command | Behavior |
|---|---|
| `++addr <pad>` | Set primary address of target instrument (0–30). |
| `++addr` (no arg) | Query current address. |
| `++auto 0\|1` | Disable/enable auto-read-after-write. Default `0` (disabled). |
| `++read` | Read until EOI or EOS. Implicit after a write if `++auto 1`. |
| `++read eoi` | Read until EOI only. |
| `++read <char>` | Read until the given char (decimal byte value) or EOI. |
| `++eoi 0\|1` | Whether to assert EOI on last byte of write. Default `1`. |
| `++eos 0\|1\|2\|3` | Append CR+LF / CR / LF / nothing to outgoing writes. Default `0` (CR+LF… wait, actually check the spec). **Verify before implementing.** |
| `++eot_enable 0\|1` | Append `eot_char` to reads. |
| `++eot_char <n>` | Set EOT append byte. |
| `++read_tmo_ms <n>` | Read timeout in ms. |
| `++clr` | Send Selected Device Clear to current address. |
| `++ifc` | Pulse Interface Clear on the bus. |
| `++rst` | Reset the controller (our daemon's GPIB state, not the instrument). |
| `++ver` | Return a version string containing "Prologix" so client libraries that sniff for it work. |
| `++mode 0\|1` | Device vs controller mode. Hardware is controller-only; return error for `0`. |

### 7.3 Commands we stub (error or no-op)

`++srq`, `++spoll`, `++llo`, `++loc`, `++savecfg`, `++trg`, `++status` — respond with a Prologix-style error or a sensible default so clients don't wedge. Document which are stubbed in the README.

### 7.4 Data path

Anything not starting with `++` is forwarded to the currently-addressed instrument:
1. Strip trailing `\r`/`\n` from the client line.
2. Apply `++eos` termination policy to build the outgoing byte string.
3. Issue a GPIB write with EOI on the last byte per `++eoi` setting.
4. If `++auto` is on, immediately do a read, then emit the response followed by `\n`.

### 7.5 Single-client policy

Accept one TCP connection at a time. If a second client connects, refuse with a banner line and close. Cleaner to implement than multiplexing and safer for bench use.

---

## 8. Dependency and runtime choices

### 8.1 USB: `nusb`

Use `nusb` (<https://docs.rs/nusb>). Rationale:

- Pure Rust, no libusb system dependency — makes macOS and Linux builds trivial (`cargo build` works).
- Async-native (returns `Future`s for transfers), plays well with Tokio.
- Actively developed, clear API.
- Handles hot-plug detection.

`rusb` is the alternative. Avoid it: C dependency, sync-only by default, and its async story is worse than `nusb`'s.

**Sanity-check before committing:** write a 30-line test program that opens the 82357B in its `0x0518` state and sends a single `0xA0` vendor control transfer. If that works on both macOS and Linux with `nusb`, the rest follows.

### 8.2 Async runtime: Tokio

Use Tokio. We need concurrent bulk-IN polling and TCP I/O. Keep it single-threaded (`#[tokio::main(flavor = "current_thread")]`) — nothing here benefits from a thread pool and it keeps reasoning about state simpler.

### 8.3 Other crates (suggested, not mandatory)

- `tracing` + `tracing-subscriber` — logging. Default to `info`; `RUST_LOG=ugpibd=debug` for protocol-level tracing.
- `clap` (derive) — CLI args.
- `anyhow` for app errors, `thiserror` for library-layer errors.
- `bytes` — byte buffer management, optional.

No serialization crate needed; everything is ASCII text or hand-rolled binary.

---

## 9. Cross-platform notes

### 9.1 Linux

- **Kernel driver conflict:** if the user has the kernel `agilent_82357a` module loaded (either from mainline staging or from `linux-gpib`), it will claim the device and we can't open it. Solutions, in priority order:
  1. Document how to blacklist the module (`/etc/modprobe.d/blacklist-gpib.conf` containing `blacklist agilent_82357a`).
  2. In `nusb`, attempt `detach_kernel_driver()` before claiming the interface. On Linux this works.
  3. Detect the conflict and emit a clear error message pointing to (1).
- **Permissions:** the device node is owned by root by default. Ship a udev rule (`99-ugpibd.rules`):
  ```
  SUBSYSTEM=="usb", ATTR{idVendor}=="0957", ATTR{idProduct}=="0518", MODE="0660", GROUP="plugdev", TAG+="uaccess"
  SUBSYSTEM=="usb", ATTR{idVendor}=="0957", ATTR{idProduct}=="0718", MODE="0660", GROUP="plugdev", TAG+="uaccess"
  ```
  Both PIDs are needed (pre- and post-firmware). `TAG+="uaccess"` grants access to the logged-in seat user, which is usually what people want.
- **Systemd unit:** provide one (`ugpibd.service`) running as a dedicated user in `plugdev`. `Restart=on-failure`.

### 9.2 macOS

- **No kernel driver conflict:** macOS has no built-in driver for the 82357. Good.
- **No permissions issue by default:** `nusb` uses IOKit; any user can open non-HID USB devices without entitlements.
- **Caveat: device matching.** If another app (Keysight IO Libraries for Mac, if the user installed it) has claimed the device, opening will fail. Detect and report.
- **LaunchAgent / LaunchDaemon:** not needed for v1. Document `brew services` or running from a terminal.

### 9.3 Windows (not a target, but avoid painting ourselves into a corner)

Windows needs a WinUSB driver binding via Zadig or similar. `nusb` supports this but we won't test it. Don't add Windows-only code.

---

## 10. Repo and module layout

```
ugpibd/
├── Cargo.toml
├── LICENSE                             # GPL-3.0
├── README.md
├── docs/
│   ├── DESIGN.md                       # this file
│   └── PROLOGIX-COMPAT.md              # which ++ commands we support
├── firmware/
│   ├── measat_releaseX1.8.hex          # embedded via include_bytes!
│   └── LICENSE                         # firmware redistribution notice
├── contrib/
│   ├── 99-ugpibd.rules                  # Linux udev
│   └── ugpibd.service                   # systemd unit
├── src/
│   ├── main.rs                         # CLI, config, top-level wiring
│   ├── lib.rs                          # re-exports for integration tests
│   ├── usb.rs                          # nusb device open, hotplug, claim
│   ├── firmware.rs                     # Intel-HEX parse + FX2 upload
│   ├── protocol.rs                     # 82357 packet enum, encode/decode
│   ├── gpib.rs                         # high-level GPIB ops on top of protocol
│   ├── prologix.rs                     # TCP line parser + command dispatch
│   └── server.rs                       # TCP listener, per-connection loop
└── tests/
    ├── hex_parse.rs                    # no hardware needed
    ├── protocol_roundtrip.rs           # no hardware needed
    └── prologix_parse.rs               # no hardware needed
```

Keep `protocol.rs` and `gpib.rs` hardware-free (they operate on byte buffers and a `trait Transport`). Real USB I/O lives behind the trait, which lets us unit-test with a mock.

---

## 11. Phased implementation plan

Each phase should end with a working binary that does something demonstrable.

### Phase 0 — `nusb` sanity check (½ day)

Tiny standalone binary that enumerates USB devices, finds `0957:0518` or `0957:0718`, and prints descriptors. Confirms `nusb` works on the target machines and that the dongle is visible.

### Phase 1 — firmware upload (1–2 days)

Implement `firmware.rs`. On finding a `0957:0518` device:
1. Parse the embedded HEX blob.
2. Execute the FX2 upload sequence.
3. Wait for renumeration (poll for `0957:0718` with a 5 s timeout).
4. Handle the double-upload quirk.

Success criterion: after running, `lsusb` / `ioreg` shows PID `0x0718` and the green READY LED is lit. No SCPI traffic yet.

### Phase 2 — protocol layer + `*IDN?` (2–3 days)

Implement `protocol.rs` (packet types from the kernel header) and enough of `gpib.rs` to:
1. Initialize the controller (send IFC, assert REN, set our PAD to 0).
2. Address a single instrument as listener, write `*IDN?\n` with EOI.
3. Address the same instrument as talker, read until EOI.
4. Print the response.

Success criterion: connected to a real instrument at a known PAD, the binary prints the IDN string. This is the moment of truth; most remaining work is protocol glue.

### Phase 3 — Prologix TCP server (2 days)

Implement `prologix.rs` and `server.rs`. Minimum viable command set: `++addr`, `++auto`, `++read`, `++ver`, plus data passthrough. Enough to run:

```python
import pyvisa
rm = pyvisa.ResourceManager("@py")
inst = rm.open_resource("TCPIP::localhost::1234::SOCKET",
                        read_termination="\n", write_termination="\n")
inst.write("++mode 1"); inst.write("++addr 15"); inst.write("++auto 1")
print(inst.query("*IDN?"))
```

### Phase 4 — the rest of Prologix, error handling, packaging (2–3 days)

Remaining `++` commands, proper timeouts, clean shutdown on SIGTERM, udev rule, systemd unit, README with quickstart, CI (GitHub Actions running the no-hardware tests on Linux and macOS runners).

### Phase 5 (optional, defer) — SRQ and VXI-11

Only if somebody actually needs them.

---

## 12. Gotchas and landmines

A non-exhaustive list of things that will bite the agent if not anticipated:

1. **Double firmware upload** on the 82357B (§5.3). Silent data-path failure if skipped.
2. **Renumeration race.** After firmware upload, the `0x0518` device handle becomes invalid immediately. Don't reuse it. Poll for `0x0718` with a bounded timeout.
3. **Kernel driver claims the device** on Linux if `agilent_82357a.ko` is loaded. Detect and `detach_kernel_driver` or emit an actionable error.
4. **USB transfer timeouts vs GPIB timeouts are different.** The USB bulk transfer should have a timeout longer than the GPIB operation — typically USB timeout = GPIB timeout + 1 s. Otherwise you'll see USB errors when the real issue is an instrument that didn't respond.
5. **Short reads are normal.** GPIB reads complete on EOI, which can happen mid-buffer. Don't assume bulk IN returns a full buffer.
6. **Byte ordering of multi-byte fields in packets.** The kernel driver uses little-endian (x86). Rust defaults matter less than matching the wire format — use explicit `u16::to_le_bytes()` / `u32::to_le_bytes()`.
7. **Don't hardcode endpoint addresses.** Read them from the interface descriptor. The firmware version can affect numbering.
8. **Never leave a pending bulk-IN transfer without a consumer.** It will back-pressure the device and you'll see "stuck" behavior. Always either poll or explicitly cancel.
9. **SCPI instrument quirks:** some old HP instruments want `\r\n` line endings, some want just `\n`. Prologix default is CR+LF (`++eos 0`). Match the default exactly or users' existing scripts will break.
10. **The firmware bug where the first upload silently fails but the device appears to work for enumeration.** You need to check PID after upload, not just "device reappeared."
11. **macOS may present the device with a different `bInterfaceNumber` ordering.** Don't assume interface 0; enumerate.
12. **Clones exist.** Cheap 82357B clones from eBay/AliExpress behave mostly identically but have been known to have firmware-loading quirks (some ship with firmware pre-flashed, so they come up as `0x0718` directly). The logic in §5.4 handles this; don't add special cases for clones.
13. **SIGINT during firmware upload leaves the device in a weird half-loaded state.** Trap signals, try to hold the 8051 in reset before exiting, and document "unplug and replug if you ctrl-C during first run."
14. **PyVISA's `TCPIP::...::SOCKET` resource is what you want for testing, NOT `TCPIP::...::INSTR`.** The latter speaks VXI-11 (which we don't implement). Document this in the README; users will get it wrong.

---

## 13. Testing strategy

### 13.1 Unit tests (no hardware)

- Intel-HEX parser: synthetic records, bad checksums, EOF handling, oversized inputs.
- Protocol encode/decode: round-trip every packet variant.
- Prologix line parser: all `++` commands, malformed input, boundary whitespace, command-vs-data disambiguation.

### 13.2 Integration tests (no hardware)

Mock `Transport` trait in `gpib.rs`. Drive a full `++addr 15 ; *IDN?` cycle against a mock that echoes a canned IDN response. Validates the Prologix ↔ GPIB ↔ protocol wiring without USB.

### 13.3 Hardware tests (manual, documented)

In `docs/HARDWARE-TEST.md`, list a reproducible checklist:
1. Firmware upload from cold.
2. `*IDN?` against a known instrument (any SCPI source, even a cheap multimeter).
3. Long read (>1 KB) to exercise multi-packet read path.
4. Timeout behavior (address a PAD with no instrument attached).
5. `++clr` followed by `*RST`.
6. Disconnect-mid-session recovery (unplug during idle; daemon should exit cleanly).

### 13.4 CI

GitHub Actions: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`. Run on `ubuntu-latest` and `macos-latest`. No hardware-required tests in CI.

---

## 14. Open questions for the maintainer

Items the agent should surface rather than guess on:

1. **Default TCP port.** 1234 is the Prologix default. Keep it, or use 1234 as default and let `--port` override?
2. **Multi-adapter support** in v1 — yes or no? If yes, config file; if no, single `--vid:pid` override flag for edge cases.
3. **Logging destination.** stderr-only, or optional syslog on Linux / os_log on macOS?
4. **Telemetry/metrics.** Probably not, but decide now so the architecture doesn't need retrofitting.
5. **Whether to advertise a fake serial number in `++ver`** — some client libraries sniff for specific Prologix firmware versions. Might need to return something like `Prologix GPIB-USB Controller version 6.107` verbatim.

---

## 15. Success criteria for v1

1. Fresh clone, `cargo build --release`, run the binary with no args on Linux or macOS with an 82357B plugged in.
2. Binary uploads firmware, opens a TCP listener on `:1234`, logs "ready" within 5 seconds.
3. A PyVISA `TCPIP::...::SOCKET` session can `*IDN?` a real instrument and get a correct response.
4. Works on an Ubuntu kernel that does not ship `CONFIG_GPIB_*` modules (the whole point).
5. Does not require root at runtime (udev rule handles permissions on Linux; macOS needs nothing).
6. Total codebase under ~2500 lines of Rust.

That's the bar. Everything else is gravy.
