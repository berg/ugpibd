# ugpibd

Userspace Rust daemon for USB-to-GPIB adapters that otherwise need an
out-of-tree kernel driver. It exposes the bus over **HiSLIP** (IVI-6.1, port
4880) and **VXI-11** (port 9010) so pyvisa and NI-VISA reach it with an
ordinary `TCPIP::...::INSTR` resource string — with locking, clear, trigger,
REN, SRQ, per-call timeouts and abort. VXI-11 is the highest-fidelity
transport for mapping GPIB devices onto the network: every bus operation,
including the client-driven read, is an explicit message on the wire. A
Prologix-compatible port is available for older scripts, and the optional
`ugpibd-portmap` package provides port-111 discovery for NI/Keysight VISA,
cooperating with system rpcbind where present.

## Supported adapters

| Backend id | Adapter | USB id |
|------------|---------|--------|
| `agilent-82357b` | Agilent/Keysight 82357B | `0957:0518` → `0957:0718` |
| `agilent-82357a` | Agilent 82357A | `0957:0007` → `0957:0107` |
| `ni-usb-hs` | NI GPIB-USB-HS | `3923:709b` |
| `ni-usb-hs` | NI GPIB-USB-HS+ | `3923:7618` |
| `ni-usb-hs` | KUSB-488A | `3923:725c` |
| `ni-usb-hs` | MC-USB-488 | `3923:725d` |

The second USB id is what the adapter enumerates as once ugpibd has uploaded
its firmware, which it does automatically. KUSB-488A and MC-USB-488 are
untested, but take the same code path as the GPIB-USB-HS.

The adapter is picked with `--backend`; the default, `auto`, detects a single
connected adapter by USB id.

## Install

You get three binaries: the `ugpibd` daemon, the `ugpibd-scpi` client, and
`ugpibd-portmap` (VXI-11 discovery on port 111 — shipped separately as the
optional `ugpibd-portmap` apt package). The apt package also installs udev
rules for adapter access, systemd units, and `/etc/default/ugpibd`.

**macOS 12+ and Linux — Homebrew:**

```bash
brew install berg/ugpibd/ugpibd
```

**Debian 12+ / Ubuntu 22.04+ / 64-bit Raspberry Pi OS — apt:**

```bash
sudo install -d /etc/apt/keyrings \
  && sudo curl -fsSLo /etc/apt/keyrings/ugpibd.asc \
       https://berg.github.io/ugpibd/apt/ugpibd-archive-keyring.asc \
  && echo "deb [signed-by=/etc/apt/keyrings/ugpibd.asc] https://berg.github.io/ugpibd/apt stable main" \
       | sudo tee /etc/apt/sources.list.d/ugpibd.list >/dev/null \
  && sudo apt update \
  && sudo apt install -y ugpibd
```

**From source** (Rust 1.75+):

```bash
cargo build --release
sudo cp contrib/60-ugpibd.rules /usr/lib/udev/rules.d/
sudo groupadd -f ugpibd && sudo usermod -aG ugpibd "$USER"
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then check the adapter is visible and start the daemon:

```bash
ugpibd --list
ugpibd
```

It binds `127.0.0.1` and serves HiSLIP (4880) and VXI-11 (9010). Use
`--bind 0.0.0.0` for remote access, `--enable-prologix` for the Prologix
port, and `RUST_LOG=ugpibd=debug` for protocol tracing.

## Connecting with VISA

```python
import pyvisa
rm = pyvisa.ResourceManager("@py")
inst = rm.open_resource("TCPIP::localhost,9010::gpib0,15::INSTR")
print(inst.query("*IDN?"))
```

Resource-string syntax, where `15` is the instrument's GPIB primary
address:

| Transport | Resource string |
|-----------|-----------------|
| **VXI-11 (recommended)** | `TCPIP::<host>,9010::gpib0,15::INSTR` |
| VXI-11, daemon's default PAD | `TCPIP::<host>,9010::inst0::INSTR` |
| VXI-11, the interface itself | `TCPIP::<host>,9010::gpib0::INSTR` |
| HiSLIP | `TCPIP::<host>::hislip15::INSTR` |

VXI-11 is the recommended transport: it is the highest-fidelity mapping of
GPIB onto the network, with every bus operation — including the
client-driven read — an explicit message on the wire. Prefer the explicit
`,9010` port form even though it matches the daemon's default: the string
then works with no portmapper involved, on any port the daemon is
configured for (`--vxi11-port`). Omitting the port
(`TCPIP::<host>::gpib0,15::INSTR`) makes the client look the port up via
the portmapper — see the `ugpibd-portmap` note below — and is the form NI
and Keysight VISA produce on their own.

The HiSLIP sub-address may be written `hislip<N>`, `gpib<N>`, or bare
`<N>`; `hislip0` means the daemon's `--default-address`, as does VXI-11's
`inst0`. Locking, service requests, and the rest of each protocol's
semantics: [docs/VXI11.md](docs/VXI11.md) and
[docs/HISLIP.md](docs/HISLIP.md).

## VXI-11 details

Same instruments and same locking as HiSLIP (a lock taken over either
protocol excludes the other), with the full protocol surface: client-driven
reads, per-call timeouts, abort, SRQ events, and — via a bare `gpib0` link —
the interface itself (bus-wide clear, raw command bytes, bus status from
the live control lines, unaddressed data transfer).

For clients that discover VXI-11 through the portmapper instead of a port
in the resource string (NI and Keysight VISA) — Linux only, systemd:

```bash
sudo apt install ugpibd-portmap
sudo systemctl enable --now ugpibd-portmap
```

It registers with system rpcbind when one is running and serves port 111
itself when none is. Details and conformance notes:
[docs/VXI11.md](docs/VXI11.md).

## `ugpibd-scpi`

A REPL bundled with the daemon, speaking any of its front-ends
(`--transport hislip|vxi11|prologix`, default hislip).

```bash
ugpibd-scpi --addr 15                       # instrument at GPIB address 15
ugpibd-scpi --host bench-pi                 # no --addr: daemon's default PAD
ugpibd-scpi --transport vxi11 --addr 18     # e.g. for ++read, below
printf '++ren 1\n*RST\n*IDN?\n' | ugpibd-scpi --addr 15
```

A line the quote-aware hint calls a query (a `?` outside string literals) is
sent and its reply printed; any other line is written without reading. The
address is fixed for the session.

| Command | Action |
|---------|--------|
| `++read` | explicit addressed read (vxi11/prologix) |
| `++clr` | Selected Device Clear |
| `++trg` | GPIB trigger (GET) |
| `++ren <0\|1>` | remote / local — semantics differ per transport, see `++help` |
| `++status` | print the serial-poll status byte |
| `++help` | full reference, including per-transport caveats |

## Prologix

`--enable-prologix` adds a Prologix GPIB-USB-compatible ASCII server on port
1234 for scripts written against `prologix-gpib-async` or raw sockets. Supported
`++` commands and their quirks: [docs/PROLOGIX.md](docs/PROLOGIX.md).

## Capturing plots and prints

Instruments that plot or print to GPIB do not answer queries — they drive the
bus at a listener, and until one exists they emit nothing at all. Two modes
cover the two ways they do it, and which one an instrument needs is a property
of the instrument, not a preference:

| The instrument | Mode | Example |
|---|---|---|
| goes talk-only and drives the bus | `++lon 1` | HP 53310A, which prints PCL raster |
| addresses a plotter at a configured address | `++dev <addr>` | SRS SR620, which plots HP-GL to address 5 |

Both stream raw bytes to `--capture-port`, with no framing and no
interpretation:

```
ugpibd --enable-prologix --capture-port 1235
printf '++mode 1\n++dev 5\n' | nc -q1 localhost 1234    # or ++lon 1
nc localhost 1235 > plot.hpgl                            # then press PLOT
```

`--listen-only` and `--listen-address <addr>` set the same modes at startup for
a unit dedicated to capture; both are switchable at runtime, so a
socket-activated daemon is never stuck in the wrong one.

Two things worth knowing before you debug a silent capture:

* **Device mode gives up system control.** No REN, no IFC, and the daemon is no
  longer controller-in-charge, so ordinary instrument traffic is refused until
  you leave the mode.
* **A capture holds the bus.** Other clients see up to one read timeout of
  added latency, and a capture client that stops reading stalls the talker —
  which on GPIB blocks every device, not just that transfer.

`++lines` dumps the eight bus control lines and is the first thing to reach for
when nothing arrives: it distinguishes an instrument that is silent from one
that is talking to somebody else. See [docs/CAPTURE.md](docs/CAPTURE.md).

## Running as a service (Linux)

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

## Multiple adapters

With one adapter attached, `auto` just works. When several are present, list
them and pick one by its physical USB port:

```bash
$ ugpibd --list
#   backend          vid:pid    port           serial             product
0   agilent-82357b   0957:0718  1-1.1          (none)             82357B
1   agilent-82357b   0957:0718  1-1.2          (none)             82357B

$ ugpibd --usb-port 1-1.2
```

The port id identifies the physical socket, not the unit: on Linux the sysfs
name (`1-1.2`), on macOS the IOKit location id (`0x03440000`). It is stable
across replug into the same port and across firmware reload. One service
instance drives one adapter, so put `--usb-port` in `UGPIBD_OPTS`.

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
