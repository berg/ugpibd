# Captured instrument output

Real bytes off the GPIB bus, kept because they cannot be regenerated without
the instrument. Intended as fixtures for whatever ends up rendering them.

## `53310a-print.pcl`

HP 53310A modulation domain analyzer, PRINT pressed with the instrument in
talk-only, captured 2026-08-06 through a NI GPIB-USB-HS+ (see `docs/CAPTURE.md`
§14.12).

**PCL raster, not HP-GL.** The instrument prints a bitmap to a PCL printer
rather than plotting vectors to a plotter, which is why it has no
plotter-address setting:

```
ESC * r 640 S     raster width, 640 pixels
ESC * r A         start raster graphics
ESC * b 74 W      transfer one row, 74 bytes   (x59 in this capture)
```

44690 bytes, 558 raster rows, captured through the capture port (`++lon 1`
plus `--capture-port`) with the adapter addressing itself as sole listener.

**Complete.** The stream ends with `ESC*rB` (end raster graphics) followed by
form feeds — every earlier attempt truncated with no end marker. Contains two
PRINT presses back to back, which is why the raster-start and raster-end counts
do not match one another: splitting a stream into pages is the client's job,
not the daemon's (`docs/CAPTURE.md` §5).

## `sr620-plot.hpgl`

SRS SR620 universal counter, PRINT pressed with plotter mode on and plotter
address 5, captured 2026-08-06 through a NI GPIB-USB-HS+ in **device mode**
(`++dev 5`) — see `docs/CAPTURE.md` §14.18.

Real HP-GL vectors, unlike the 53310A's PCL raster:

```
DF;SC-30,255,-20,205;TL2;SR.8,2;SP1;PU0,200;PD0,0,250,0,...
```

4408 bytes: 268 `PA`, 252 `PD`, 29 `LB`, 22 `CP`, 21 `PR`, 9 `XT`, 7 `YT`,
5 `PU`, 2 `SP`, 2 `LT`, and one each of `DF`, `SC`, `TL`, `SR`.

**No output instructions** — no `OI`, `OP`, `OE`, `OS`. The instrument never
asks the plotter anything, so no persona is required to satisfy it. It ends
`PU-30,-20;SP0;` — pen up, pen away, which is the natural end-of-plot marker a
client should frame on.
