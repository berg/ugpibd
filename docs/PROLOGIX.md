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

### ugpibd extensions

Not Prologix commands. They exist because `++srq` answers one bit of a register
whose other seven bits are what you need when a capture is silent, and because
capture has to be reachable at runtime — the daemon is usually socket- or
systemd-activated, so a mode settable only by restarting with a different flag
would not be settable in practice.

| Command | Effect |
|---|---|
| `++lines` | Dump the eight bus control lines, e.g. `0x31 NDAC NRFD REN` |
| `++lon [0\|1]` | Unaddressed listen, for a talk-only instrument. No argument queries |
| `++dev [addr\|off]` | Act as a GPIB *device* at `addr` instead of as a controller. No argument queries |

`++lon` and `++dev` are mutually exclusive with ordinary traffic: writes are
refused while either is on, because in listen-only the RFD holdoff is released
and in device mode the daemon is not the controller at all. Leaving either mode
re-initialises the adapter, which pulses IFC — that is the point, since
reclaiming the bus is what leaving means.

See [CAPTURE.md](CAPTURE.md) for which instruments need which, and
[ROADMAP.md](ROADMAP.md) for the remaining gaps.

## Hardware limitations (adapter firmware)

- `++mode 0` returns an error. This is *not* because the hardware is
  controller-only — `++dev` above is a working device mode on the NI adapters,
  and an SR620 plots to it. It is that Prologix device mode also implies
  dumping received data inline on the same connection, which `--capture-port`
  does instead. See ROADMAP item 3.
- No secondary addressing
- 8-bit EOS comparison only
