# Hardware Test Checklist

Manual tests requiring a physical 82357B and a SCPI instrument.

## Test 1: Firmware upload from cold

1. Unplug the 82357B, wait 5 seconds, replug.
2. Confirm `lsusb` shows `0957:0518`.
3. Run `RUST_LOG=ugpibd=debug ugpibd`.
4. Confirm log shows "holding 8051 in reset", firmware record writes,
   "device came up as 0x0718".
5. Confirm `lsusb` shows `0957:0718` and only the green READY LED is lit.

## Test 2: *IDN? round-trip

1. Connect a SCPI instrument (e.g. Keysight 34461A) at PAD 15.
2. Start ugpibd.
3. `nc localhost 1234`, type: `++addr 15`, `++auto 1`, `*IDN?`
4. Confirm instrument IDN string is returned.

## Test 3: Long read (>1 KB)

1. Connect instrument, request a large dataset (e.g. `FETCH?` after a long
   measurement).
2. Confirm all bytes arrive correctly.

## Test 4: Timeout behavior

1. Set `++addr 29` (nothing attached).
2. Send `*IDN?` with `++auto 1`.
3. Confirm a timeout error is logged and connection stays alive.

## Test 5: ++clr and *RST

1. `++addr 15`, `++clr`, confirm instrument displays SYS RESET or equivalent.
2. `*RST`, confirm instrument returns to factory defaults.

## Test 6: Disconnect mid-session

1. Start ugpibd, connect a client.
2. Unplug the 82357B while idle.
3. Confirm daemon logs disconnect and exits cleanly (exit code 0 or 1, no panic).
