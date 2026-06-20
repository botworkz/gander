# Gander SVG trace candidates

Five candidate SVG traces of `gander.png` for visual comparison **before
choosing one to replace the PNG asset.** All produced with the new
`mcp-svg-trace` server (botworkz/mcp-extra#195 + #196) which wraps the
pure-Rust [`vtracer`](https://github.com/visioncortex/vtracer) crate.

Workflow:
1. `magick_convert` to resize / quantise the source PNG.
2. `svg_trace` to vectorise into Bézier paths.

The source `gander.png` is 769×769, 31 460 unique colours, with full
alpha. Tracing the raw source produces ~200 path elements with chunky
quantised gradients; quantising down to 3 colours first (your stated
intent — "looks like an svg already and has 3 colours") collapses the
trace to ~15 paths and a much smaller file.

| File | Source step | Trace step | Paths | Size |
|---|---|---|---|---|
| `gander-quantised-3.svg` | 512 px + `magick -colors 3 +dither` | `svg_trace filter_speckle=8 layer_difference=32` | 15 | 18 KB |
| `gander-quantised-3-768.svg` | 768 px + `magick -colors 3 +dither` | `svg_trace filter_speckle=6 layer_difference=48` | 17 | 26 KB |
| `gander-poster-256.svg` | 256 px (full colour) | `svg_trace preset=poster` | 200 | 65 KB |
| `gander-photo-512.svg` | 512 px (full colour) | `svg_trace preset=photo` | 86 | 86 KB |
| `gander-tuned-512.svg` | 512 px (full colour) | `svg_trace filter_speckle=6 color_precision=7 layer_difference=24` | 211 | 112 KB |

My recommendation:

* If the gander is genuinely 3-colour and you want a clean icon-like
  SVG, **`gander-quantised-3-768.svg`** — 17 paths, 26 KB, traces from
  the higher-res quantised intermediate so the path geometry has more
  detail than the 512 px version. Smallest file that still looks like
  the original silhouette + ink.
* `gander-quantised-3.svg` is the same idea at 512 px source — slightly
  jaggier, but 18 KB.
* The poster / photo / tuned variants are kept for comparison only;
  they're each bigger than the original PNG and the chunky-bezier look
  on smooth regions reads as "wrong" rather than "vector".

Once you pick one, drop a follow-up PR replacing `gander.png` with the
chosen SVG and pointing the two `<img src="/assets/gander.png">` sites
(`crates/gander-chat/src/acp_core/components/{message_list,message_view}.rs`)
at the new path. This directory should be deleted in that same PR — it
exists only to load the candidates onto a branch for visual review.
