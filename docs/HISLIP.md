# HiSLIP front-end

The HiSLIP (IVI-6.1) server listens on port 4880, the IANA-assigned port.

## Sub-addresses

The sub-address selects the GPIB primary address, and may be written
`hislip<N>`, `gpib<N>`, or a bare `<N>`. A bare `hislip0` / `gpib0` means "use
the daemon's configured default PAD" (`--default-address`, default 0).

Why no comma in the sub-address: pyvisa-py parses `hislip0,15` as
`sub_address=hislip0, port=15` — it would try to open TCP port 15 rather than
passing 15 through to the server. Embedding the PAD in the sub-address itself
(`hislip15`) avoids that.

## Locking

`viLock` / `viUnlock` are enforced, not advisory. An exclusive lock (empty lock
string) is granted only when nobody else holds anything; a shared lock is
granted to everyone using the same lock string. A request that conflicts waits
out the timeout the caller asked for before being refused.

While a lock is held, another client's reads, writes, triggers and device
clears are **left unprocessed until the lock frees**, rather than interleaved
or refused. That is what the spec calls for — HiSLIP has no "resource locked"
message and none is to be invented — so a locked-out client blocks and, if it
runs out of patience, times out. Its status queries, lock info and maximum
message size still work, which is how it can find out what is going on.

Locks nest, they are scoped to the instrument rather than the bus — locking
`hislip23` leaves `hislip3` free — and they are released when the session
closes, so a client that crashes holding one does not lock the instrument out.

## Service requests

`viWaitOnEvent(VI_EVENT_SERVICE_REQ)` works, including for requests that depend
on MAV. Those need help, because HiSLIP has no read request — the server pushes
replies — so a GPIB bridge must drain the instrument's output queue on its own
initiative, and that read is exactly what clears MAV. Nothing looking afterwards
would ever see it.

The daemon watches the SRQ line across its own read rather than trying to
second-guess the instrument. Nothing parses `*SRE` out of the traffic: applying
that mask is the instrument's job, and a sniffed copy would be a cache with no
invalidation — blind to the front panel, to another controller on the bus, to
`*PSC`, and to the several NRf spellings of the same number. So if the
instrument pulled SRQ, there is a service request; if it did not, there is not.
With `*SRE 16` set, the classic `write(query)` → wait for SRQ → `read()` idiom
works here as it does against a native HiSLIP instrument.

One consequence worth knowing: when the daemon serial-polls to fill in the
status byte, that poll clears RQS at the instrument, so a client reading the
status byte itself would find the bit already taken. The daemon hands over what
it consumed on the next `AsyncStatusQuery`, once.

This needs an adapter that can report SRQ asynchronously, which both the NI
GPIB-USB-HS and the 82357B do.
