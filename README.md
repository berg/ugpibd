# ugpibd

Userspace Rust daemon for USB-to-GPIB adapters that otherwise need an
out-of-tree kernel driver. Exposes two TCP front-ends against the same bus:

- **Prologix-compatible** ASCII protocol on port 1234 (configurable)
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
| `ni-usb-hs` | NI GPIB-USB-HS / HS+ (and KUSB-488A, MC-USB-488 clones), VID `0x3923` | **Experimental — translated from the kernel driver, not yet tested on hardware** |

## Requirements

- Linux (Ubuntu 24.04+) or macOS 12+
- A supported USB-GPIB adapter (see above)
- Rust 1.75+

## Quick Start

```bash
cargo build --release
sudo cp contrib/99-ugpibd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
./target/release/ugpibd
```

For protocol-level tracing:

```bash
RUST_LOG=ugpibd=debug ./target/release/ugpibd
```

## If the kernel driver interferes (Linux)

If you see "failed to claim interface 0", the matching kernel GPIB module may be
loaded (`agilent_82357a` for the 82357B, `ni_usb_gpib` for the NI adapters).
Blacklist whichever applies:

```bash
echo "blacklist agilent_82357a" | sudo tee /etc/modprobe.d/blacklist-gpib.conf
echo "blacklist ni_usb_gpib" | sudo tee -a /etc/modprobe.d/blacklist-gpib.conf
sudo modprobe -r agilent_82357a ni_usb_gpib
```

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
daemon's configured default PAD" (`--hislip-default-pad`, default 14).

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

## Interactive `scpi` client

`scpi` is a small REPL bundled with the daemon. It speaks **HiSLIP** to
`ugpibd` (the same transport pyvisa uses), so it does not need the Prologix
port.

```bash
# Talk to the instrument at GPIB primary address 15:
scpi --addr 15
# Or omit --addr to use the daemon's default PAD (sub-address hislip0):
scpi --host bench-pi --port 4880
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
printf '++ren 1\n*RST\n*IDN?\n' | scpi --addr 15
```

## Supported `++` commands

The following applies to the **Prologix** server (port 1234), not the
`scpi` client above.


Implemented: `++addr`, `++auto`, `++read`, `++eoi`, `++eos`, `++eot_enable`,
`++eot_char`, `++read_tmo_ms`, `++clr`, `++ifc`, `++rst`, `++ver`, `++mode`.

Stubbed (no-op or constant response): `++srq`, `++spoll`, `++llo`, `++loc`,
`++savecfg`, `++trg`, `++status`.

## Hardware limitations (firmware)

- Controller-only (no device mode — `++mode 0` returns an error)
- No secondary addressing
- 8-bit EOS comparison only

## License

The daemon is GPL-3.0-or-later. The firmware blob under `firmware/` is
redistributed from https://github.com/fmhess/linux_gpib_firmware; see
`firmware/LICENSE` for its terms.

The HiSLIP message codec and protocol definitions in `src/hislip/` are
adapted from [lxi-rs](https://github.com/Atmelfan/lxi-rs) (GPL-3.0-or-later,
© Gustav Palmqvist).
