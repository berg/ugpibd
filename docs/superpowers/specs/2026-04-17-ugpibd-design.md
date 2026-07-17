# ugpibd — Implementation Spec

**Date:** 2026-04-17  
**Status:** approved  
**Source design:** `docs/DESIGN.md` (read that first for hardware background and reference sources)

---

## Decisions resolved during brainstorming

| Question | Decision |
|----------|----------|
| Multi-adapter support in v1 | Single-adapter only. Architecture stays multi-adapter-friendly (USB device + TCP listener as one instantiable unit) but no config-file or multi-port wiring in v1. |
| TCP port | Default `1234`, overridable with `--port`. |
| Logging | `tracing` to stderr only. `RUST_LOG=ugpibd=debug` for protocol-level tracing. No syslog/os_log. |
| Metrics/telemetry | None. No seam left open. |
| `++ver` response | `Prologix GPIB-USB Controller version 6.107` — verbatim fake to satisfy client library sniffing. |
| `++eos` default | `0` (append CR+LF), matching real Prologix hardware. Values: 0=CR+LF, 1=CR, 2=LF, 3=nothing. |
| USB async strategy | Native `nusb` futures driven directly in the Tokio `current_thread` runtime (no `spawn_blocking` wrapper). |

---

## Module layout

```
src/
  main.rs          CLI (clap), config, top-level wiring; starts USB init then TCP listener
  lib.rs           Re-exports for integration tests
  usb.rs           nusb device open, interface claim, kernel-driver detach, hotplug wait
  firmware.rs      Intel-HEX parser + FX2 vendor-request upload sequence
  protocol.rs      82357 packet enum (DATA_PIPE_CMD_*), encode/decode, unit-tested
  gpib.rs          High-level GPIB ops (write, read, ifc, clr, set_addr, ren) over trait Transport
  prologix.rs      TCP line parser, ++ command state machine, data passthrough logic
  server.rs        TCP listener, single-client enforcement, per-connection async loop

tests/
  hex_parse.rs          No hardware: synthetic HEX records, bad checksums, edge cases
  protocol_roundtrip.rs No hardware: round-trip every packet variant
  prologix_parse.rs     No hardware: all ++ commands, malformed input, data vs command disambiguation
```

---

## Dependency direction

```
main.rs
  └─ server.rs
       └─ prologix.rs
            └─ gpib.rs
                 └─ protocol.rs
                      └─ (trait Transport)
  └─ usb.rs  ──── implements Transport ────────────────┘
  └─ firmware.rs (called once at startup by usb.rs)
```

`protocol.rs` and `gpib.rs` are hardware-free. All USB I/O is behind `trait Transport`:

```rust
trait Transport {
    async fn write(&self, data: &[u8]) -> Result<()>;
    async fn read(&self, buf: &mut [u8]) -> Result<usize>;
}
```

---

## Data flow (happy path)

1. **TCP client → prologix.rs**: line read from socket, classified as `++command` or SCPI data.
2. **`++addr 15`**: updates prologix state only — no USB I/O.
3. **`*IDN?\n`**: prologix applies `++eos` termination, calls `gpib::write(pad, bytes, eoi)`.
4. **gpib.rs**: emits TMS9914-style register writes to address the instrument (ATN, talk/listen), then `DATA_PIPE_CMD_WRITE` packet via `protocol::encode()` → `Transport::write()`.
5. **usb.rs**: fires bulk-OUT transfer, awaits bulk-IN response, decodes via `protocol::decode()`, returns `Ok(())` or typed error.
6. **If `++auto 1`**: gpib.rs issues `DATA_PIPE_CMD_READ`, assembles multi-chunk response until EOI into `Vec<u8>`.
7. **Response path**: prologix.rs appends `eot_char` if `++eot_enable 1`, writes bytes + `\n` to TCP socket.

---

## Firmware upload sequence

1. Find `0x0957:0x0518`. If not found, check for `0x0957:0x0718` (already loaded) — skip upload.
2. Hold 8051 in reset: vendor request `0xA0`, address `0xE600`, data `[0x01]`.
3. Parse embedded `measat_releaseX1.8.hex` (via `include_bytes!`), write each DATA record via vendor request `0xA0`.
4. Release reset: vendor request `0xA0`, address `0xE600`, data `[0x00]`.
5. Wait for renumeration: poll for `0x0957:0x0718` with 5 s timeout.
6. **Double-upload quirk (82357B):** if device reappears as `0x0518`, repeat steps 2–5 once. After two attempts still `0x0518` → fatal error with clear message.

---

## Prologix command surface

**Implemented:**

| Command | Behavior |
|---------|----------|
| `++addr [n]` | Get/set target instrument primary address (0–30) |
| `++auto 0\|1` | Disable/enable auto-read-after-write (default `0`) |
| `++read [eoi\|<char>]` | Read until EOI, EOS, or given byte value |
| `++eoi 0\|1` | Assert EOI on last write byte (default `1`) |
| `++eos 0\|1\|2\|3` | Outgoing line termination: CR+LF/CR/LF/none (default `0`) |
| `++eot_enable 0\|1` | Append `eot_char` to reads (default `0`) |
| `++eot_char <n>` | Byte to append when `eot_enable 1` (default `0`) |
| `++read_tmo_ms <n>` | Read timeout in ms |
| `++clr` | Selected Device Clear to current address |
| `++ifc` | Pulse Interface Clear |
| `++rst` | Reset daemon GPIB state (not the instrument) |
| `++ver` | Returns `Prologix GPIB-USB Controller version 6.107` |
| `++mode 0\|1` | Controller-only hardware; `++mode 0` returns error |

**Stubbed (error or no-op):** `++srq`, `++spoll`, `++llo`, `++loc`, `++savecfg`, `++trg`, `++status`

---

## Error handling

- `thiserror` for typed errors in `protocol.rs`, `firmware.rs`, `gpib.rs`.
- `anyhow` at the application layer (`main.rs`, `server.rs`).
- USB transfer timeout = GPIB timeout + 1 s (avoids USB errors masking instrument non-response).
- Kernel driver conflict on Linux: attempt `detach_kernel_driver()`; if denied, emit actionable error pointing to blacklist instructions.
- SIGTERM/SIGINT: trap signals, attempt to hold 8051 in reset before exit during firmware upload, then exit cleanly.

---

## Platform specifics

- **Linux:** ship `contrib/99-ugpibd.rules` (both PIDs, `MODE=0660`, `GROUP=plugdev`, `TAG+="uaccess"`) and `contrib/ugpibd.service`.
- **macOS:** no special permissions needed. Detect if Keysight IO Libraries have claimed the device and report clearly.
- **Endpoint numbers:** never hardcoded — read from interface descriptor at runtime.

---

## Testing

- **Unit (no hardware):** hex parser, protocol round-trips, prologix line parser — in `tests/`.
- **Integration (no hardware):** mock `Transport` impl drives full `++addr` + `*IDN?` cycle.
- **CI:** `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` on `ubuntu-latest` + `macos-latest`.
- **Hardware checklist:** documented in `docs/HARDWARE-TEST.md` (firmware upload, IDN, long read, timeout, clr+rst, disconnect recovery).

---

## Rust version / edition

- Edition 2021, MSRV Rust 1.75+. This allows native `async fn` in traits (no `async-trait` crate needed).

---

## Constraints

- Target codebase: under ~2500 lines of Rust.
- No `linux-gpib`, no kernel modules, no `fxload` runtime dependency.
- Firmware blob embedded via `include_bytes!` — no install-time asset management.
- Single `cargo build --release` → working binary on Linux and macOS.
