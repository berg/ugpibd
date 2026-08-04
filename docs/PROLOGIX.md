# Prologix front-end

A Prologix GPIB-USB-compatible ASCII server on port 1234, for scripts already
written against `prologix-gpib-async`, `plx-gpib-ethernet`, or raw sockets.
It is opt-in: start the daemon with `--enable-prologix`.

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
`++eot_char`, `++read_tmo_ms`, `++clr`, `++ifc`, `++rst`, `++ver`, `++mode`,
`++spoll [pad]`, `++trg [pad]`, `++srq`, `++loc [pad]`, `++llo`.

Accepted and ignored: `++savecfg`, `++status`.

`++srq` reads the live SRQ line, so it needs a backend that can report bus
state; on adapters that cannot (currently everything except `ni-usb-hs`) it
logs a warning and returns nothing rather than inventing a "0".

`++loc` sends an addressed Go To Local, so it returns one instrument to its
front panel and leaves the rest of the bus in remote — but the next write to
that instrument addresses it again and puts it straight back into remote, which
is what IEEE-488 says happens. `++llo` sends Local Lockout, which is universal:
it disables the local key on every device on the bus, and dropping REN is the
only way to clear it.

See [ROADMAP.md](ROADMAP.md) for the remaining gaps.

## Hardware limitations (adapter firmware)

- Controller-only — no device mode, so `++mode 0` returns an error
- No secondary addressing
- 8-bit EOS comparison only
