#!/usr/bin/env python3
"""Reconstruct an I2C EEPROM image from a logic-analyser capture of the boot read.

The FX2 reads its whole firmware out of the EEPROM at power-on, so a capture of
SDA/SCL spanning the plug-in contains the bytes. This decodes the bus and
reassembles them by EEPROM address.

    i2c_eeprom_recover.py capture.csv --scl 1 --sda 2 -o eeprom.bin

Input is a CSV whose first column is time and whose other columns are channels
(Saleae and PulseView digital exports both look like this). Values may be 0/1 or
analog; anything above --threshold counts as high.
"""
import argparse, csv, sys

def load(path, scl_col, sda_col, threshold):
    rows = []
    with open(path, newline="") as f:
        for row in csv.reader(f):
            if not row:
                continue
            try:
                t = float(row[0])
                scl = float(row[scl_col]) > threshold
                sda = float(row[sda_col]) > threshold
            except (ValueError, IndexError):
                continue          # header or short line
            rows.append((t, scl, sda))
    return rows

def decode(rows):
    """Yield ('start',) / ('stop',) / ('byte', value, ack) in bus order."""
    events, bits, cur = [], 0, 0
    in_frame = False
    p_scl, p_sda = None, None
    for _, scl, sda in rows:
        if p_scl is None:
            p_scl, p_sda = scl, sda
            continue
        if scl and p_scl:                      # SCL steady high: look for S/P
            if p_sda and not sda:
                events.append(("start",)); bits, cur, in_frame = 0, 0, True
            elif not p_sda and sda:
                events.append(("stop",));  bits, cur, in_frame = 0, 0, False
        elif scl and not p_scl:                # rising edge: sample
            if in_frame:
                if bits < 8:
                    cur = (cur << 1) | int(sda)
                    bits += 1
                else:
                    events.append(("byte", cur, not sda))   # ACK is active low
                    bits, cur = 0, 0
        p_scl, p_sda = scl, sda
    return events

def rebuild(events):
    """Walk transactions, tracking the EEPROM's auto-incrementing pointer."""
    image, ptr, phase, addr_bytes = {}, None, "idle", []
    reads = writes = 0
    for ev in events:
        if ev[0] == "start":
            phase, addr_bytes = "addr", []
            continue
        if ev[0] == "stop":
            phase = "idle"
            continue
        if ev[0] != "byte":
            continue
        val = ev[1]
        if phase == "addr":
            if val & 1:                        # read: continue from pointer
                phase, reads = "read", reads + 1
            else:
                phase, writes = "word", writes + 1
            continue
        if phase == "word":                    # 16-bit word address, big-endian
            addr_bytes.append(val)
            if len(addr_bytes) == 2:
                ptr = (addr_bytes[0] << 8) | addr_bytes[1]
                addr_bytes = []
            continue
        if phase == "read" and ptr is not None:
            image[ptr] = val
            ptr = (ptr + 1) & 0x7FFF           # 24LC256 wraps at 32K
    return image, reads, writes

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("capture")
    ap.add_argument("--scl", type=int, required=True, help="SCL column index")
    ap.add_argument("--sda", type=int, required=True, help="SDA column index")
    ap.add_argument("--threshold", type=float, default=0.5)
    ap.add_argument("-o", "--out", default="eeprom_sniffed.bin")
    ap.add_argument("--size", type=lambda s: int(s, 0), default=0x8000)
    a = ap.parse_args()

    rows = load(a.capture, a.scl, a.sda, a.threshold)
    if not rows:
        sys.exit("no samples parsed — check the column indices")
    print(f"{len(rows)} samples")
    events = decode(rows)
    print(f"{sum(1 for e in events if e[0]=='start')} starts, "
          f"{sum(1 for e in events if e[0]=='byte')} bytes on the bus")
    image, reads, writes = rebuild(events)
    print(f"{reads} read transactions, {writes} address writes")
    if not image:
        sys.exit("no EEPROM data recovered — are SCL and SDA the right way round?")

    lo, hi = min(image), max(image)
    out = bytearray(b"\xff" * a.size)
    for addr, val in image.items():
        if addr < a.size:
            out[addr] = val
    open(a.out, "wb").write(out)
    print(f"recovered {len(image)} bytes, {lo:#06x}..{hi:#06x} "
          f"({100*len(image)/a.size:.1f}% of {a.size:#x})")
    print(f"wrote {a.out}")
    if image.get(0) in (0xC0, 0xC2):
        print(f"first byte {image[0]:#04x}: a valid FX2 boot EEPROM")
    elif 0 in image:
        print(f"warning: first byte is {image[0]:#04x}, expected 0xC0 or 0xC2")
    gaps = [a_ for a_ in range(lo, hi + 1) if a_ not in image]
    if gaps:
        print(f"note: {len(gaps)} bytes in that range were never read "
              f"(first gap {gaps[0]:#06x}) — the FX2 only reads what it needs")

if __name__ == "__main__":
    main()
