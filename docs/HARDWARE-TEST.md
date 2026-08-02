# Hardware Test Checklist

Manual tests requiring a physical adapter and a SCPI instrument. They apply to
any supported adapter; where a step names a USB product id, use the row for the
adapter under test:

| Adapter | Pre-firmware | After firmware upload |
|---------|--------------|-----------------------|
| 82357B  | `0957:0518`  | `0957:0718`           |
| 82357A  | `0957:0007`  | `0957:0107`           |
| NI GPIB-USB-HS | — (no upload step) | `3923:709b`  |

`ugpibd --list` reports the ids on every platform and is the easiest check;
`lsusb` (Linux) and `system_profiler SPUSBDataType` (macOS) also work.

Tests drive the HiSLIP front-end with the bundled `ugpibd-scpi` client, which
is what `ugpibd` serves by default. The Prologix front-end is opt-in and is
covered separately at the end.

Most of this checklist is automated in `contrib/`, and running those first will
save going through it by hand:

```bash
contrib/hardware_exercise.py --pad <PAD>      # Tests 3-8, one instrument
contrib/multidev_srq.py --a <PAD> --b <PAD>   # two instruments, SRQ + isolation
```

Test with **two instruments on the bus** whenever you can. A single-device bus
cannot show addressing faults at all — it is structurally incapable of it — and
that blind spot hid writes being delivered to every previously-addressed
listener until a second instrument was attached.

Throughout, `<PAD>` is the instrument's GPIB primary address. Read it off the
instrument's own configuration menu rather than scanning for it: writing to an
empty address stalls the GPIB source handshake for the full timeout, so a sweep
of the bus is slow and tells you nothing the front panel doesn't.

## Test 1: Firmware upload from cold

1. Unplug the adapter, wait 5 seconds, replug.
2. Confirm `ugpibd --list` shows the pre-firmware product id.
3. Run `RUST_LOG=ugpibd=debug ugpibd`.
4. Confirm the log shows "holding 8051 in reset", a firmware record count,
   "releasing 8051 from reset", and "device came up as" the post-firmware id.
5. Confirm `ugpibd --list` now shows the post-firmware id, that the serial and
   product columns show the adapter's own strings (pre-firmware they are
   deliberately blank), and that only the green READY LED is lit.

## Test 2: Warm start

1. With the adapter already firmware-loaded from Test 1, restart `ugpibd`.
2. Confirm the log says it is skipping the upload ("already firmware-loaded")
   and that the controller still initializes.

## Test 3: `*IDN?` round-trip

1. Connect a SCPI instrument at `<PAD>` and start `ugpibd`.
2. `echo '*IDN?' | ugpibd-scpi --addr <PAD>`
3. Confirm the instrument's IDN string is returned.

## Test 4: Long read (>1 KB)

1. Request a dataset larger than 1 KB in a single transfer. On a 34401A:

   ```
   printf '*RST\nCONF:VOLT:DC 10\nVOLT:DC:NPLC 0.02\nSAMP:COUN 200\nREAD?\n' \
     | ugpibd-scpi --addr <PAD>
   ```

2. Confirm every reading arrives and parses — 200 comma-separated values,
   roughly 3.2 KB.

## Test 5: Trigger, device clear, and REN

1. Bus trigger (GPIB GET). On a 34401A:

   ```
   printf '*RST\nCONF:VOLT:DC 10\nSAMP:COUN 5\nTRIG:SOUR BUS\nINIT\n++trg\nFETC?\n' \
     | ugpibd-scpi --addr <PAD>
   ```

   Confirm 5 readings come back — `FETC?` returning nothing means the GET
   never reached the instrument.
2. Device clear. Confirm it has an *effect* — that the instrument still
   answers afterwards proves nothing, since a clear that does nothing at all
   leaves it answering perfectly well. That is exactly how a device clear
   addressed to the wrong role went unnoticed. On a 34401A, an initiated
   measurement rejects a second `INIT` with `-213,"Init ignored"`, and a
   working clear aborts it so the second `INIT` succeeds:

   ```
   CONF:VOLT:DC 10 / TRIG:SOUR BUS / INIT / INIT / SYST:ERR?   -> -213  (control)
   CONF:VOLT:DC 10 / TRIG:SOUR BUS / INIT / ++clr / INIT / SYST:ERR?  -> +0
   ```

   The control arm matters: without it a probe that can never fail looks like
   a pass.
3. `++ren 0` then `++ren 1`, and confirm the REMOTE annunciator follows.

## Test 6: Serial poll (status byte)

The point of this test is that the status byte is *read from the instrument*.
A status of 0 is a legitimate answer ("nothing to report"), so a backend that
had no serial poll and simply returned 0 would look identical to a working one.
Provoke a **non-zero** status and check it is reported.

1. Set a status bit in whatever way the instrument supports. On a 34401A,
   enable the Standard Event summary and cause a command error — that
   instrument does not surface a non-empty error queue in the status byte, so
   `*ESE` is what makes the bit visible:

   ```
   printf '*CLS\n*ESE 32\nBOGUSCMD\n*STB?\n++status\n' | ugpibd-scpi --addr <PAD>
   ```

2. Confirm `++status` matches the instrument's own `*STB?` — both `32` here.
3. Send `*CLS` and confirm both return to `0`.

If `++status` reports 0 while `*STB?` reports non-zero, the serial poll is not
reaching the bus. Note that serial-polling an address with *no* device returns
0 promptly rather than timing out, because undriven GPIB lines read as zero —
an absent instrument and a status of 0 are not distinguishable this way.

## Test 7: Service request (SRQ)

Unlike every other test here, this one checks a message the daemon sends
*unsolicited*. It needs a client that can wait on a VISA service-request event;
released `pyvisa-py` cannot, so use a build whose HiSLIP backend implements it.

1. Configure the instrument to request service, and wait for the event. On a
   34401A, `*ESE 32` summarises a command error into ESB and `*SRE 32` turns
   that summary into SRQ:

   ```python
   import pyvisa
   SRQ = pyvisa.constants.EventType.service_request
   inst = pyvisa.ResourceManager("@py").open_resource(
       "TCPIP::127.0.0.1::hislip<PAD>::INSTR")
   inst.write("*CLS"); inst.write("*ESE 32"); inst.write("*SRE 32")
   inst.enable_event(SRQ, pyvisa.constants.EventMechanism.queue)
   inst.write("BOGUSCMD")            # provoke the error -> SRQ
   inst.wait_on_event(SRQ, 5000)     # should return promptly
   ```

2. Confirm the event arrives (well under the 5 s timeout).
3. With `RUST_LOG=ugpibd=debug`, confirm the daemon logs `forwarding service
   request` with a status byte that has bit 6 (RQS, `0x40`) set — `stb=96` for
   the sequence above, which is RQS + ESB.

If the log instead says `srq raised by another device on the bus`, the poll
came back without RQS: the SRQ was someone else's, which is exactly the case
the forwarder is meant to filter out. `adapter cannot report SRQ` means the
backend has no notification path at all — no supported adapter is in that state
now that both the NI GPIB-USB-HS and the 82357B report SRQ asynchronously.

## Test 8: Timeout and bus recovery

1. Query an address with nothing attached: `echo '*IDN?' | ugpibd-scpi --addr 5`
2. Confirm a timeout is logged and the daemon stays up.
3. Immediately query the real instrument again and confirm it still answers.
   Repeat the dead-address query a few times first — a failed transfer must not
   leave a device addressed as talker and wedge the next transaction.

## Test 9: Disconnect mid-session

1. Start `ugpibd` and connect a client.
2. Unplug the adapter **while idle** — the interesting case, because nothing is
   in flight to fail and notice.
3. Confirm the daemon logs `was unplugged, shutting down` within a second or so
   and exits with status **0**, no panic.
4. Replug and start it again; on an 82357 this re-uploads firmware from cold.

Exit status 0 is deliberate rather than incidental. `contrib/ugpibd.service`
sets `Restart=on-failure`, so a clean exit stops systemd restarting the daemon
into an adapter that is not there, while the udev rule in
`contrib/60-ugpibd.rules` (`ACTION=="add"`) starts it again when one is plugged
in. Exiting non-zero here would produce a restart loop for as long as the
adapter stayed unplugged.

The daemon deliberately does not try to tidy up the adapter on this path — every
transfer would fail, and warning about failing to reset hardware that has been
removed is only noise.

## Test 10: Go To Local and Local Lockout

**Confirmed on an 82357B with an HP 34401A**, every step below behaving as
described. The command bytes are pinned by unit tests on both backends and were
read off the wire on that adapter and on an NI GPIB-USB-HS — `UNL LAD<pad> GTL`
addressed, a bare unaddressed `LLO`.

Do not try to substitute a host-side measurement for the panel; it was tried and
does not work. A 34401A in local services *queries* about twelve times slower,
but serial polls are answered below the SCPI layer and run at ~500/s either way,
and any query re-addresses the instrument as a listener, which returns it to
remote and erases the evidence before it can be timed. Watching the annunciator
is the measurement, which is why this stays a manual test.

Unlike REN, these are per-device (GTL) and bus-wide (LLO), so watch the panel
rather than the daemon log.

Two things about a 34401A to have straight first, or the results read as
nonsense: in remote its front-panel keys are **already** dead, all except
LOCAL — so "the keys do nothing" proves nothing on its own. LOCAL is the only
interesting key, because without a lockout it returns the instrument to local
and under LLO it stops working too.

Send one thing at a time and look before sending the next. In particular do not
query between steps: addressing the instrument is itself what returns it to
remote, which undoes the state you are trying to observe.

1. `++ren 1`, then any query, so the instrument is in remote: RMT lit.
2. `++loc` — RMT should go dark **while REN stays asserted**. That is the part
   dropping REN cannot do: every other instrument on the bus stays in remote.
3. Query it again. RMT comes back on by itself. Correct, not a bug: addressing
   a device to listen while REN is asserted returns it to remote, so GTL lasts
   only until the next write.
4. Press LOCAL (Shift on a 34401A) with no lockout in force: RMT goes dark, so
   the key is live to begin with.
5. `++ren 1`, then `++llo`, then press LOCAL again: now it should do **nothing**
   and RMT should stay lit. That is LLO, and it is the one thing driving REN
   alone could never do.
6. `++ren 0`. RMT goes dark and LOCAL works again — dropping REN is the only
   way to clear a lockout, since IEEE-488 defines no un-LLO command.

`hislip-stress/bench_gtl.py` walks this one prompt at a time over HiSLIP.

Over HiSLIP the same paths are reached through `viGpibControlREN` with
`VI_GPIB_REN_ADDRESS_GTL` (6) and `VI_GPIB_REN_ASSERT_LLO` (4).

## Optional: Prologix front-end

The Prologix-compatible port is disabled unless `--enable-prologix` is passed.
With it running, `nc localhost 1234` then `++addr <PAD>`, `++auto 1`, `*IDN?`
should return the same IDN string as Test 3. `contrib/smoke_test.py --pad <PAD>`
exercises both front-ends and reports any mismatch between them.

## Validated configurations

- **82357B** (`MY47100427`) + HP 34401A at PAD 23 and HP 53132A at PAD 3, on
  macOS: Tests 1–9 pass, `hardware_exercise.py` 12/12, `multidev_srq.py` 9/9.
  Writes reach only their addressed instrument in both directions.
- **82357A** (`MY45181868`) + the same two instruments, on macOS: same results.
  Firmware upload from cold succeeded on the first attempt (487 records),
  twice, including once after an unplug/replug mid-session. A soak of 300 short
  queries, 20 long reads, and 30 session open/closes completed with no
  failures.
- **82357B** (`0957:0518` cold) + HP 34401A at PAD 23 and HP 53132A at PAD 3, on
  macOS: firmware uploaded from cold, including the documented double-upload
  retry. HiSLIP conformance 25/25 and all 9 `hislip-stress` scripts pass,
  covering locking, MAV-driven service requests and the remote/local codes.
  GTL and LLO reach the bus with the same command bytes as the NI adapter, and
  Test 10 was walked on the front panel: addressed GTL takes the instrument
  local while REN stays asserted, re-addressing returns it to remote, and LOCAL
  works until LLO is sent and not after.
- **NI GPIB-USB-HS** + SR620 at PAD 16, and + HP 34401A at PAD 23 on macOS:
  HiSLIP conformance and the `hislip-stress` suite pass, including SRQ push and
  MAV-driven service requests.

Two instruments asserting SRQ at almost the same moment used to lose the
second request: SRQ is a wired-OR line and the adapter notifies on a
transition, so a device asserting while the line is already low produces no new
edge. The forwarder now re-polls while the line stays asserted, using the level
read `srq_asserted`, which both backends implement. Worth re-checking on a
two-instrument bus after any change to the SRQ path.

## A note on VISA clients

Released `pyvisa-py` does not implement `read_stb`, `assert_trigger`, or
service-request events over HiSLIP — it raises `VI_ERROR_NSUP_OPER` or
`NotImplementedError`. Tests 5–7 therefore cannot be run through a stock
pyvisa install. Either use `ugpibd-scpi` (`++status`, `++trg`), or a pyvisa-py
build whose HiSLIP backend implements them.

A HiSLIP write completes when the bytes are handshaken onto the bus, not when
the instrument has parsed them. Reading the status byte immediately after a
write can therefore observe the state from *before* that write. Poll until the
expected bit appears rather than asserting on a single read — a real script
would poll anyway.
