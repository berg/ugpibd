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
