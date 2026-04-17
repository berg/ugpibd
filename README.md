# gpibd

Userspace Rust daemon for the Agilent/Keysight 82357B USB-to-GPIB adapter.
Exposes a Prologix-compatible TCP server on port 1234 (configurable).

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

Use `TCPIP::...::SOCKET` (not `::INSTR` — that speaks VXI-11 which we don't
implement):

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
