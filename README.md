# ugpibd

Userspace Rust daemon for USB-to-GPIB adapters that otherwise need an
out-of-tree kernel driver. Exposes two TCP front-ends against the same bus:

- **Prologix-compatible** ASCII protocol on port 1234 (opt-in via `--enable-prologix`)
- **HiSLIP** (IVI-6.1) on port 4880 (the IANA-assigned HiSLIP port)

Use HiSLIP with pyvisa/NI-VISA for proper `TCPIP::...::INSTR` resource
strings with locking, clear, trigger, and REN. Use the Prologix port for
existing scripts written against `prologix-gpib-async` or raw sockets.

## Supported adapters

The adapter is selected with `--backend` (default `auto`, which detects a
single connected adapter by USB VID/PID). Run `ugpibd --backend list` to see
the ids.

| Backend id | Adapter | Status |
|------------|---------|--------|
| `agilent-82357b` | Agilent/Keysight 82357B (USB `0957:0518` → `0957:0718` after firmware) | Supported |
| `agilent-82357a` | Agilent 82357A (USB `0957:0007` → `0957:0107` after firmware) | Supported — firmware is bundled and uploaded automatically |
| `ni-usb-hs` | NI GPIB-USB-HS (USB `3923:709b`) | Supported |
| `ni-usb-hs` | KUSB-488A (`3923:725c`), MC-USB-488 (`3923:725d`) | Untested, but byte-for-byte the same code path as the GPIB-USB-HS |
| `ni-usb-hs` | NI GPIB-USB-HS+ (`3923:7618`) | **Experimental** — different endpoints and an extra vendor-request init, both implemented but never run on hardware |

### Multiple adapters

With one adapter attached, `auto` just works. When several are present, list
them and pick one by its physical USB port:

```bash
$ ugpibd --list
#   backend          vid:pid    port           serial             product
0   agilent-82357b   0957:0718  1-1.1          (none)             82357B
1   agilent-82357b   0957:0718  1-1.2          (none)             82357B

$ ugpibd --usb-port 1-1.2
```

The **port id** identifies the physical socket, not the unit: on Linux it is the
sysfs name (`1-1.2`), on macOS the IOKit location id (`0x03440000`). It is stable
across replug into the same port and across firmware reload. Serial numbers are
shown for reference only — this hardware's serials are not reliably unique, so
they are never used to select an adapter.

## Requirements

- Linux (Ubuntu 22.04+, Debian 12+, or 64-bit Raspberry Pi OS) or macOS 12+
- A supported USB-GPIB adapter (see above)
- Rust 1.75+ to build from source

## Install

Installing gets you two binaries — the `ugpibd` daemon and the `ugpibd-scpi`
client. The Linux packages additionally set up udev rules for adapter access,
systemd units, and `/etc/default/ugpibd`.

### Homebrew (macOS and Linux)

```bash
brew install berg/ugpibd/ugpibd
```

One command is enough: the fully qualified name taps
[berg/homebrew-ugpibd](https://github.com/berg/homebrew-ugpibd) automatically,
and trusts this formula alone rather than the whole tap. The formula installs
prebuilt binaries straight from the release tarballs, so there is nothing to
compile.

Later releases arrive with `brew upgrade`. Note that the *short* name is not
trusted, so `brew install ugpibd` and `brew upgrade ugpibd` will not resolve —
use the qualified name, or opt in once:

```bash
brew trust --formula berg/ugpibd/ugpibd
```

Homebrew installs the binaries only — no udev rules, no systemd unit. On Linux
that means you either run the daemon as a user with access to the adapter, or use
the apt packages below, which wire all of that up.

### Debian / Ubuntu / Raspberry Pi OS (apt)

Add the repository and install, in one command:

```bash
sudo install -d /etc/apt/keyrings \
  && sudo curl -fsSLo /etc/apt/keyrings/ugpibd.asc \
       https://berg.github.io/ugpibd/apt/ugpibd-archive-keyring.asc \
  && echo "deb [signed-by=/etc/apt/keyrings/ugpibd.asc] https://berg.github.io/ugpibd/apt stable main" \
       | sudo tee /etc/apt/sources.list.d/ugpibd.list >/dev/null \
  && sudo apt update \
  && sudo apt install -y ugpibd
```

That fetches the signing key into `/etc/apt/keyrings/`, registers the repository
with `signed-by=` so the key is trusted for this repository only — never
system-wide, which is what the deprecated `apt-key add` used to do — and
installs the daemon. Upgrades then arrive through `apt upgrade` as normal.

Afterwards:

```bash
ugpibd --list          # is the adapter visible?
sudo systemctl start ugpibd
```

Packages depend only on glibc 2.34+, so they install on Ubuntu 22.04+ and
Debian 12+. Every published release stays in the pool, so you can pin or roll
back:

```bash
apt list -a ugpibd                 # versions available
sudo apt install ugpibd=<version>  # pin one
```

Package list and the deb822 (`.sources`) form of the setup:
<https://berg.github.io/ugpibd/>

**Raspberry Pi:** use 64-bit Raspberry Pi OS, Bookworm or later — check with
`dpkg --print-architecture`, which should report `arm64`. The only
shared-library dependency is glibc, since the USB layer is pure Rust with no
libusb. There is no `armhf` package, so 32-bit Pi OS needs a build from source.

The daemon package is all you need. If a kernel GPIB driver turns out to be
claiming your adapter, there is a separate opt-in package for that — see [If the
kernel driver interferes](#if-the-kernel-driver-interferes-linux).

**The service does not start on its own** — see [Running as a
service](#running-as-a-service) below.

### Single .deb

If you would rather not add a repository, the `.deb` files are attached to every
[release](https://github.com/berg/ugpibd/releases/latest):

```bash
sudo apt install ./ugpibd_*_amd64.deb
```

Use `apt install ./…` rather than `dpkg -i`, so dependencies are resolved. The
optional `ugpibd-blacklist-linux-gpib_*_all.deb` is attached to the same release.

### From source

```bash
cargo build --release
sudo cp contrib/60-ugpibd.rules /usr/lib/udev/rules.d/
sudo groupadd -f ugpibd && sudo usermod -aG ugpibd "$USER"
sudo udevadm control --reload-rules && sudo udevadm trigger
./target/release/ugpibd
```

The rules file sets `GROUP="ugpibd"` and tags the device `uaccess`, so the user
at the local console gets access without being in that group. Log out and back
in after `usermod` for remote or non-console sessions.

## Quick Start

```bash
ugpibd
```

By default the daemon binds to `127.0.0.1` and runs only the HiSLIP
front-end. Pass `--bind 0.0.0.0` (or a specific interface address) for
remote access, and `--enable-prologix` to also expose the Prologix port.

For protocol-level tracing:

```bash
RUST_LOG=ugpibd=debug ugpibd
```

## Running as a service

The packaged `ugpibd.service` is deliberately not started or enabled on
install — grabbing a USB device and opening a TCP port is not something a
package should do behind your back. Pick whichever of these fits:

| Goal | How |
|------|-----|
| Run it once now | `sudo systemctl start ugpibd` |
| Start when an adapter is plugged in | set `UGPIBD_AUTOSTART=yes` in `/etc/default/ugpibd` |
| Start at boot regardless of hardware | `sudo systemctl enable --now ugpibd` |

Daemon options go in `UGPIBD_OPTS` in `/etc/default/ugpibd`:

```sh
UGPIBD_OPTS="--bind 0.0.0.0 --enable-prologix --default-address 15"
```

Both settings take effect on the next start with no `systemctl daemon-reload`,
because the file is read when the service starts rather than when the unit is
loaded. `/etc/default/ugpibd` is a conffile, so your edits survive upgrades.

Autostart works by a udev rule pulling in `ugpibd-hotplug.service`, which checks
`UGPIBD_AUTOSTART` and starts the daemon only if it is set to `yes`. When it is
not set, that unit exits cleanly rather than failing, so nothing accumulates in
`systemctl --failed`. Startup on plug also covers adapters already attached at
boot, since udev re-emits their `add` events during early boot.

With more than one adapter attached, `auto` detection is ambiguous — pin one
with `--usb-port` (from `ugpibd --list`) in `UGPIBD_OPTS`. One service instance
drives one adapter.

## If the kernel driver interferes (Linux)

If you see "failed to claim interface 0", the matching kernel GPIB driver has
claimed the adapter: `agilent_82357a` for the 82357A/B, `ni_usb_gpib` for the NI
adapters. These ship in the kernel (`drivers/gpib`, mainline since Linux 6.13)
and in out-of-tree linux-gpib builds.

On Debian/Ubuntu, install the optional package:

```bash
sudo apt install ugpibd-blacklist-linux-gpib
```

Or do it by hand:

```bash
printf 'blacklist agilent_82357a\nblacklist ni_usb_gpib\n' \
    | sudo tee /etc/modprobe.d/ugpibd-blacklist-linux-gpib.conf
sudo modprobe -r agilent_82357a ni_usb_gpib
```

A deliberate `modprobe` still works either way, so this does not permanently
lock you out of linux-gpib. Note that blacklisting `ni_usb_gpib` also disables
the kernel driver for the NI GPIB-USB-B, which ugpibd does **not** support —
module granularity is coarser than device granularity, so it cannot be
exempted.

## PyVISA usage

### HiSLIP (recommended)

```python
import pyvisa
rm = pyvisa.ResourceManager("@py")
# Sub-address encodes the GPIB primary address: "hislip<PAD>".
inst = rm.open_resource("TCPIP::localhost::hislip15::INSTR")
print(inst.query("*IDN?"))
```

The HiSLIP server accepts sub-addresses of the form `hislip<N>`,
`gpib<N>`, or a bare `<N>`. A bare `hislip0` / `gpib0` means "use the
daemon's configured default PAD" (`--default-address`, default 0).

Why no comma in the sub-address: pyvisa-py parses
`hislip0,15` as `sub_address=hislip0, port=15` — it would try to open
TCP port 15 rather than passing 15 through to the server. Embedding the
PAD in the sub-address itself (`hislip15`) avoids that.

### Prologix (legacy)

```python
import pyvisa
rm = pyvisa.ResourceManager("@py")
inst = rm.open_resource(
    "TCPIP::localhost::1234::SOCKET",
    read_termination="\n",
    write_termination="\n",
)
inst.write("++mode 1")
inst.write("++addr 15")
inst.write("++auto 1")
print(inst.query("*IDN?"))
```

## Interactive `ugpibd-scpi` client

`ugpibd-scpi` is a small REPL bundled with the daemon. It speaks **HiSLIP** to
`ugpibd` (the same transport pyvisa uses), so it does not need the Prologix
port.

```bash
# Talk to the instrument at GPIB primary address 15:
ugpibd-scpi --addr 15
# Or omit --addr to use the daemon's default PAD (sub-address hislip0):
ugpibd-scpi --host bench-pi --port 4880
```

Each line is a request/response round-trip: a line containing `?` is sent as
a query and its reply is printed; any other line is written without reading.
`--addr N` is encoded as the HiSLIP sub-address `hislip<N>` at connect time;
the address is fixed for the session.

Meta-commands map to HiSLIP control operations:

| Command | Action |
|---------|--------|
| `++clr` | Selected Device Clear |
| `++trg` | GPIB trigger (GET) |
| `++ren <0\|1>` | REN off / on |
| `++status` | print the serial-poll status byte |
| `++help` | list meta-commands |

Non-TTY stdin is supported for scripting:

```bash
printf '++ren 1\n*RST\n*IDN?\n' | ugpibd-scpi --addr 15
```

## Supported `++` commands

The following applies to the **Prologix** server (port 1234), not the
`ugpibd-scpi` client above.


Implemented: `++addr`, `++auto`, `++read`, `++eoi`, `++eos`, `++eot_enable`,
`++eot_char`, `++read_tmo_ms`, `++clr`, `++ifc`, `++rst`, `++ver`, `++mode`,
`++spoll [pad]`, `++trg [pad]`, `++srq`.

`++srq` reads the live SRQ line, so it needs a backend that can report bus
state; on adapters that cannot (currently everything except `ni-usb-hs`) it
logs a warning and returns nothing rather than inventing a "0".

Accepted and ignored: `++llo`, `++loc`, `++savecfg`, `++status`.

See [docs/ROADMAP.md](docs/ROADMAP.md) for the remaining gaps, including
asynchronous SRQ notification.

## Hardware limitations (firmware)

- Controller-only (no device mode — `++mode 0` returns an error)
- No secondary addressing
- 8-bit EOS comparison only

## Origin and relationship to linux-gpib

The USB adapter backends in `src/backend/` are **not** original protocol
implementations. They were translated — re-expressed in Rust — from the
in-kernel GPIB drivers under `drivers/gpib/`, as of the **Linux v7.0** release
(`VERSION = 7`, `PATCHLEVEL = 0`, `SUBLEVEL = 0`). That subsystem is the
mainline home of the [linux-gpib] project by Frank Mori Hess. Concretely:

- `agilent-82357a` / `agilent-82357b` ← `drivers/gpib/agilent_82357a/`, plus
  the shared `tms9914` controller logic
- `ni-usb-hs` ← `drivers/gpib/ni_usb/ni_usb_gpib.c`, plus `nec7210` / `tnt4882`
  register definitions

> [!IMPORTANT]
> **Report bugs here — never to the linux-gpib or kernel `drivers/gpib`
> maintainers.** ugpibd is an independent userspace re-implementation. Any
> defect you hit is a bug in *this* port: the upstream maintainers did not
> write this Rust code, cannot reproduce it, and cannot support it. Do **not**
> open issues, send email, or otherwise contact them about ugpibd. File it in
> this repository's issue tracker instead.

[linux-gpib]: https://linux-gpib.sourceforge.io/

## License

`ugpibd` as a whole is distributed under **GPL-3.0-or-later**; every source
file carries an SPDX header.

- **Translated adapter drivers** (`src/backend/`) are a derivative work of the
  in-kernel `drivers/gpib/` drivers (the linux-gpib project), copyright ©
  2001–2006 Frank Mori Hess and © 1997–2002 David A. Schleef, among others.
- **HiSLIP message codec and protocol definitions** (`src/hislip/`) are adapted
  from [lxi-rs](https://github.com/Atmelfan/lxi-rs) (GPL-3.0-or-later,
  © Gustav Palmqvist).
- **Firmware blob** (`firmware/measat_releaseX1.8.hex`) is proprietary
  Agilent/Keysight firmware, redistributed unmodified from
  [fmhess/linux_gpib_firmware](https://github.com/fmhess/linux_gpib_firmware).
  It is **not** covered by this project's GPL; see `firmware/LICENSE` for its
  redistribution terms.
