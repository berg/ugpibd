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
2. `++clr`, and confirm the instrument shows its device-clear behaviour.
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

## Test 7: Timeout and bus recovery

1. Query an address with nothing attached: `echo '*IDN?' | ugpibd-scpi --addr 5`
2. Confirm a timeout is logged and the daemon stays up.
3. Immediately query the real instrument again and confirm it still answers.
   Repeat the dead-address query a few times first — a failed transfer must not
   leave a device addressed as talker and wedge the next transaction.

## Test 8: Disconnect mid-session

1. Start `ugpibd` and connect a client.
2. Unplug the adapter while idle.
3. Confirm the daemon logs the disconnect and exits cleanly (exit code 0 or 1,
   no panic).

## Optional: Prologix front-end

The Prologix-compatible port is disabled unless `--enable-prologix` is passed.
With it running, `nc localhost 1234` then `++addr <PAD>`, `++auto 1`, `*IDN?`
should return the same IDN string as Test 3. `contrib/smoke_test.py --pad <PAD>`
exercises both front-ends and reports any mismatch between them.

## Validated configurations

- **82357B** — the original bring-up target.
- **82357A** + HP 34401A at PAD 23, on macOS: Tests 1–8 pass. Firmware upload
  from cold succeeded on the first attempt (487 records). A soak of 300 short
  queries, 20 long reads, and 30 session open/closes completed with no
  failures.
- **NI GPIB-USB-HS** + SR620 at PAD 16.

Note that `pyvisa-py` does not implement `read_stb` or `assert_trigger` over
HiSLIP, so those two operations cannot be exercised through pyvisa — use
`ugpibd-scpi`'s `++status` and `++trg` as above.
