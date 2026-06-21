# Gander SVG trace candidates

Candidate vector traces of `gander.png`. **Review only** — pick one,
then a follow-up PR will:

* Replace `gander.png` with the chosen SVG.
* Update the two `<img src="/assets/gander.png">` references in
  `crates/gander-chat/src/acp_core/components/{message_list,message_view}.rs`.
* Delete this directory.

## Candidates

| File | Pre-trace | Trace knobs | Paths | Size | Distinct fills |
|---|---|---|---|---|---|
| `gander-q6-769-sharp.svg` | `magick -colors 6 +dither` | default sharp | 34 | 21 KB | 6 (no white) |
| `gander-q7-769-sharp.svg` | `magick -colors 7 +dither` | default sharp | 34 | 22 KB | 16 (no white) |
| `gander-q8-769-sharp.svg` | `magick -colors 8 +dither` | default sharp | 38 | 23 KB | 25 (no white) |
| `gander-remap5-769-sharp.svg` | nearest-neighbour snap to fixed 5-colour palette | default sharp | 34 | 21 KB | **5 (incl. white)** |

The qN-769-sharp variants all lose the white eye-sclera because the
median-cut quantiser allocates palette slots by region area, and the
white eye is too small to win a slot against blue / black / orange.
Forcing `magick -fuzz N% -fill white -opaque white` first didn't help
— the next `-colors N` step still merged the (few) white pixels back
into the nearest blue cluster.

`gander-remap5-769-sharp.svg` works around that by skipping the
quantiser entirely: every pixel is snapped to the nearest of an
explicit five-colour palette (black, white, blue, orange, green) by
squared RGB distance. The white eye survives because it is now an
exact, repeated colour the tracer can build a path from.
