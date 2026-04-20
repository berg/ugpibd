# gpibd

Userspace Rust daemon for the Agilent/Keysight 82357B USB-to-GPIB adapter.
Exposes two TCP front-ends against the same bus:

- **Prologix-compatible** ASCII protocol on port 1234 (configurable)
- **HiSLIP** (IVI-6.1) on port 4880 (the IANA-assigned HiSLIP port)

Use HiSLIP with pyvisa/NI-VISA for proper `TCPIP::...::INSTR` resource
strings with locking, clear, trigger, and REN. Use the Prologix port for
existing scripts written against `prologix-gpib-async` or raw sockets.

## Requirements

- Linux (Ubuntu 24.04+) or macOS 12+
- An Agilent/Keysight 82357B (USB ID 0957:0518 before firmware, 0957:0718 after)
- Rust 1.75+

## Quick Start

```bash
cargo build --release
sudo cp contrib/99-gpibd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
./target/release/gpibd
```

For protocol-level tracing:

```bash
RUST_LOG=gpibd=debug ./target/release/gpibd
```

## If the kernel driver interferes (Linux)

If you see "failed to claim interface 0", the kernel `agilent_82357a` module may
be loaded. Blacklist it:

```bash
echo "blacklist agilent_82357a" | sudo tee /etc/modprobe.d/blacklist-gpib.conf
sudo modprobe -r agilent_82357a
```

## PyVISA usage

### HiSLIP (recommended)

```python
import pyvisa
rm = pyvisa.ResourceManager("@py")
# Subaddress encodes the GPIB primary address (e.g. "gpib0,15" or "hislip0,15").
inst = rm.open_resource("TCPIP::localhost::hislip0,15::INSTR")
print(inst.query("*IDN?"))
```

The HiSLIP server accepts subaddresses of the form `hislip0`, `hislip0,N`,
`gpib0,N`, or a bare `N`. If no PAD is encoded, the default from
`--hislip-default-pad` (default 14) is used.

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

## Supported `++` commands

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
