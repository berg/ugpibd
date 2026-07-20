# ugpibd — select a USB-GPIB adapter by physical port

Status: approved design (pending spec review)
Date: 2026-07-20
Target version: 0.3.0

## Summary

Support more than one attached USB-GPIB adapter and let the operator target a
specific one. Selection is by the adapter's **physical USB port**, expressed as
an OS-native "port id" — never by USB serial number (this hardware's serials are
not trustworthy: clones reuse them and some units omit them). The serial is
still *shown* in the listing for human reference; it is never used to match.

Two new CLI surfaces:

- `--list` — enumerate every attached supported adapter and exit.
- `--usb-port <PORT>` — bind to the adapter at that port id.

With no selector and exactly one adapter present, behavior is unchanged.

## Background

Today `backend::detect()` scans `nusb::list_devices()`, matches by USB VID/PID,
and **errors if more than one** supported adapter is present. `find_device()`
(per backend) re-scans and opens the *first* VID/PID match. There is no way to
pick a specific unit when several are attached, which is the whole point here
(the operator may have six).

The `nusb` 0.1.14 accessors we rely on:

- `serial_number() -> Option<&str>` — cross-platform, but untrusted here → display only.
- `sysfs_path() -> &Path` *(Linux)* — e.g. `/sys/bus/usb/devices/1-1.2`.
- `location_id() -> u32` *(macOS)* — IOKit location id, encodes bus + port chain.
- `bus_number() -> u8`, `device_address() -> u8` — **not** stable; `device_address`
  is reassigned on every enumeration.

Verified live: on macOS, devices without serials resolve cleanly by
`location_id` (e.g. `0x03440000`); on Ubuntu 6.8, `sysfs_path()` returns
`/sys/bus/usb/devices/…`. The port path is also **stable across the 82357's
firmware renumeration** (same physical socket), unlike `device_address`.

## The port id

A short, OS-native string that identifies a physical port. Stable across replug
into the same socket and across firmware renumeration; differs in shape per OS
(accepted — the operator is fine with that).

- **Linux / Android:** the sysfs node basename, i.e. `sysfs_path().file_name()`,
  e.g. `1-1.2` (root hubs appear as `usb1`).
- **macOS:** `format!("{:#010x}", location_id())`, e.g. `0x03440000`.
- **Other (incl. Windows):** best-effort `instance_id()` where available, else
  `bus{n}-addr{a}` with a "not stable" caveat. ugpibd ships only Linux + macOS;
  this branch exists so the crate still compiles elsewhere and is not a
  supported/tested path.

Implemented as `port_id(&nusb::DeviceInfo) -> String` behind `cfg`.

### Matching a user-supplied port id

`--usb-port <PORT>` is compared leniently so the operator can paste either what
`--list` shows or what the OS tools show:

- **Linux:** match if the input equals the port id (`1-1.2`), the full sysfs
  path (`/sys/bus/usb/devices/1-1.2`), or is a trailing `/`-segment of the full
  path. (So both the `--list` value and a raw `sysfs_path` copy work.)
- **macOS:** parse the input as hex (optional `0x`/`0X`, case-insensitive) and
  compare numerically to `location_id()`; fall back to exact string compare if
  it does not parse.

Implemented as `port_matches(&nusb::DeviceInfo, input: &str) -> bool` behind
`cfg`. This is the unit-tested core (pure string/number logic; no devices
needed — tests pass synthetic path strings through the Linux matcher and hex
strings through a small pure helper).

## CLI

New args on the daemon:

- `--list` (flag): print attached supported adapters and exit `0`.
- `--usb-port <PORT>` (`Option<String>`): select the adapter at this port id.

Interaction with the existing `--backend`:

| `--backend` | `--usb-port` | Result |
|---|---|---|
| `auto` (default) | *unset* | Enumerate all supported. 1 → open it; 0 → "no adapter" error; >1 → error **listing the port ids** and suggesting `--usb-port`. |
| `auto` | `X` | Open the device at port `X`; its backend kind is inferred from its VID/PID. |
| `<id>` | *unset* | Scope enumeration to that kind; same 0/1/many logic. |
| `<id>` | `X` | Open port `X`, and require its inferred kind == `<id>` (else error). |
| `list` | — | Unchanged: lists backend *kinds*, exits. |

`--list` output (one line per adapter; columns aligned):

```
#  backend          vid:pid    port          serial            product
0  agilent-82357b   0957:0718  1-1.1         MY49010203        Agilent 82357B
1  agilent-82357b   0957:0718  1-1.2         (none)            Agilent 82357B
2  ni-usb-hs        3923:702a  1-3           01A8B2C3          GPIB-USB-HS
```

Serial is shown verbatim (or `(none)`), purely informational.

## Internals

### New module `src/backend/select.rs`

- `pub enum UsbSelector { Auto, Port(String) }`
- `port_id(&DeviceInfo) -> String` and `port_matches(&DeviceInfo, &str) -> bool`
  (both `cfg`-gated per OS, as above).
- `pub struct DiscoveredAdapter { kind: BackendKind, port_id: String, vid: u16,
  pid: u16, product: Option<String>, serial: Option<String> }`
- `pub fn enumerate() -> Result<Vec<DiscoveredAdapter>>` — one pass over
  `nusb::list_devices()`; for each device, find the `BackendKind` whose
  `usb_ids()` contains its `(vid, pid)`; collect matches with their port id and
  strings. Powers `--list`.

### Reworked selection in `backend/mod.rs`

- Replace `detect()`'s "error on >1" with resolution through the selector:
  `open_selected(selector: &UsbSelector, backend: Option<&str>, timeout_ms)`:
  1. `enumerate()`, filter by `backend` kind if given, filter by `selector` if
     `Port`.
  2. Resolve to exactly one adapter; produce the helpful multi-match error
     (listing port ids) or the no-match error otherwise.
  3. Open that specific device (below).
- `main.rs` builds the `UsbSelector` from `--usb-port` and passes `--backend`
  through; `--list` short-circuits by calling `enumerate()` and printing.

### Opening a *specific* device (port-aware, multi-adapter-safe)

The backend open paths currently re-scan by VID/PID and take the first match;
that is wrong when identical units are attached. Thread the resolved port down:

- `agilent_82357::usb::find_device(model, port: Option<&str>)` — return the
  device matching `model`'s VID/PID **and** `port_matches`, if `port` is `Some`.
- `agilent_82357::usb::wait_for_renumeration(model, port, …)` — after firmware
  upload, find the re-enumerated device at the **same port id** (stable across
  renumeration) rather than keying on the old bus/address. When `port` is `None`
  (single-adapter auto case), keep today's bus+address logic unchanged so the
  well-tested single-device path is untouched.
- `ni_usb_hs::usb::find_device(port: Option<&str>)` — same VID/PID + port filter.
- The port flows from `open_selected` → `BackendKind::open(…, port)` →
  backend `open()` → `find_device`.

This makes selecting one of several identical 82357Bs correct even through the
firmware double-upload dance.

## Testing

- **Unit (pure, no hardware):** `port_matches` on Linux path forms (exact id,
  full sysfs path, trailing-segment) and the macOS hex-normalization helper
  (`0x03440000` / `03440000` / `0X…` all equal; garbage falls back to string
  compare). Selector filtering + the multi-match/no-match error messages, driven
  by a synthetic `Vec<DiscoveredAdapter>` (factor the resolution logic to take a
  slice so it is testable without USB).
- **Manual / e2e:** `--list` and `--usb-port` on macOS (live) and slopbox
  (Linux); against real adapters whenever hardware is attached. The port-aware
  renumeration path can only be fully exercised with a cold 82357 on hardware —
  documented as such.
- Existing suites must stay green; `cargo fmt` + `cargo clippy -D warnings`.

## Out of scope

- Using the serial number as a selector (explicitly excluded).
- Hotplug/rescan while running (`nusb::watch_devices` exists; not needed now).
- Windows support (compiles best-effort; untested).
- Driving more than one adapter simultaneously from a single daemon — this
  selects *which one* adapter the daemon drives, not multi-bus operation.

## Version

Minor bump to `0.3.0` (additive CLI; no breaking changes to existing flags).
