#!/usr/bin/env python3
"""Reconstruct an EEPROM image from a sigrok/PulseView I²C annotation export.

Input lines look like:
    11868063-11868525 I²C: Address/data: Data read: C2
Annotations are emitted per class and are not in time order, so they are
sorted by start sample before being walked.

    sigrok_i2c_recover.py i2c.txt -o eeprom.bin
"""
import argparse, re, sys, collections

LINE = re.compile(r"^(\d+)-(\d+)\s+.*?:\s*(?:Address/data:\s*)?(.+?)\s*$")

def parse(path):
    evs = []
    for line in open(path, errors="replace"):
        m = LINE.match(line)
        if m:
            evs.append((int(m.group(1)), m.group(3)))
    evs.sort(key=lambda e: e[0])
    return evs

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("annotations")
    ap.add_argument("-o", "--out", default="eeprom_sniffed.bin")
    ap.add_argument("--size", type=lambda s: int(s, 0), default=0x8000)
    a = ap.parse_args()

    evs = parse(a.annotations)
    if not evs:
        sys.exit("no annotations parsed")

    image = {}
    ptr = None
    mode = None          # 'w' collecting a word address, 'r' streaming data
    word = []
    kinds = collections.Counter()
    addrs = collections.Counter()

    for _, text in evs:
        if text.startswith("Address write:"):
            addrs[text.split(":")[1].strip()] += 1
            mode, word = "w", []
        elif text.startswith("Address read:"):
            addrs[text.split(":")[1].strip()] += 1
            mode = "r"
        elif text.startswith("Data write:"):
            if mode == "w":
                word.append(int(text.split(":")[1].strip(), 16))
                if len(word) == 2:
                    ptr = (word[0] << 8) | word[1]
                    word = []
        elif text.startswith("Data read:"):
            if mode == "r" and ptr is not None:
                image[ptr] = int(text.split(":")[1].strip(), 16)
                ptr = (ptr + 1) & 0x7FFF
        kinds[text.split(":")[0]] += 1

    print("annotation kinds:", dict(kinds))
    print("device addresses seen:", dict(addrs))
    if not image:
        sys.exit("no data bytes recovered")

    lo, hi = min(image), max(image)
    out = bytearray(b"\xff" * a.size)
    for addr, val in image.items():
        if addr < a.size:
            out[addr] = val
    open(a.out, "wb").write(out)

    covered = len(image)
    print(f"recovered {covered} bytes, {lo:#06x}..{hi:#06x} "
          f"({100*covered/a.size:.1f}% of {a.size:#x})")
    gaps = [x for x in range(lo, hi + 1) if x not in image]
    print(f"gaps inside that range: {len(gaps)}"
          + (f" (first {gaps[0]:#06x})" if gaps else ""))
    b0 = image.get(0)
    if b0 in (0xC0, 0xC2):
        print(f"first byte {b0:#04x}: a valid FX2 boot EEPROM")
    elif b0 is not None:
        print(f"warning: first byte {b0:#04x}, expected 0xC0/0xC2")
    print(f"wrote {a.out}")

if __name__ == "__main__":
    main()
