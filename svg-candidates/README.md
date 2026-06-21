# Gander SVG trace candidates

Candidate vector traces of `gander.png`. **Review only** — pick one,
then a follow-up PR will:

* Replace `gander.png` with the chosen SVG.
* Update the two `<img src="/assets/gander.png">` references in
  `crates/gander-chat/src/acp_core/components/{message_list,message_view}.rs`.
* Delete this directory.

## Sharp-line candidates (round 2)

You said the source is "super simple … 3 colours" and the first round
lost the sharpness. Source pre-quantised to 4–6 colours with `magick
-colors N +dither` before tracing, with vtracer's `corner_threshold` /
`length_threshold` / `filter_speckle` dropped so it tracks the original
ink edges instead of smoothing them away.

| File | Source step | Trace step | Paths | Size |
|---|---|---|---|---|
| `gander-q4-polygon.svg` | 769 px + `magick -colors 4 +dither` | `svg_trace mode=polygon corner_threshold=5 length_threshold=1 filter_speckle=1` | 26 | **5.7 KB** |
| `gander-q4-spline-fine.svg` | 769 px + `magick -colors 4 +dither` | `svg_trace mode=spline corner_threshold=5 length_threshold=1 filter_speckle=1` | 26 | 17 KB |
| `gander-q4-sharp.svg` | 769 px + `magick -colors 4 +dither` | `svg_trace corner_threshold=25 length_threshold=2 filter_speckle=2` | 26 | 19 KB |
| `gander-q5-fine.svg` | 769 px + `magick -colors 5 +dither` | `svg_trace corner_threshold=20 length_threshold=2` | 34 | 30 KB |
| `gander-q6-769-sharp.svg` | 769 px + `magick -colors 6 +dither` | `svg_trace corner_threshold=60 length_threshold=4` | 34 | 21 KB |
| `gander-q6-769-cutout.svg` | 769 px + `magick -colors 6 +dither` | `svg_trace hierarchical=cutout corner_threshold=30 length_threshold=2` | 30 | 44 KB |
| `gander-1536-q4-spline.svg` | upsample 1536 px + `magick -colors 4 +dither` | `svg_trace corner_threshold=10 length_threshold=2` | 46 | 69 KB |

Recommendation: **`gander-q4-polygon.svg`** (5.7 KB, polygon mode
preserves straight edges and sharp corners exactly; this is what you
want when the source is line-art-shaped). Open it next to the PNG in
the **Files changed** tab and judge by eye — the polygon mode trades
smooth-curve approximation for edge fidelity, which is the right trade
for an icon with crisp ink.

If polygon mode looks too faceted on the duck's beak / eye outline,
`gander-q4-spline-fine.svg` is the same trace at the same colour-count
but in spline mode — still smooth, but with the tight corner/length
thresholds so the splines actually hug the original outline instead of
rounding it off.

## First-round candidates (kept for comparison)

| File | Paths | Size |
|---|---|---|
| `gander-quantised-3.svg` | 15 | 18 KB |
| `gander-quantised-3-768.svg` | 17 | 26 KB |
| `gander-poster-256.svg` | 200 | 65 KB |
| `gander-photo-512.svg` | 86 | 86 KB |
| `gander-tuned-512.svg` | 211 | 112 KB |
