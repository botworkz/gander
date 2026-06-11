# gander

A tabbed [COSMIC](https://github.com/pop-os/cosmic-epoch) viewer for [`geese`](https://github.com/phlax/geese) profiles.

## Status

Pre-alpha. Initial scaffold; not yet built against a real COSMIC desktop.

Scoped for v0:

- Tab strip with one tab per open profile (open / close / reorder)
- "Open profile" picker listing `geese` profiles, with inline "create new" affordance
- Per-tab placeholder page showing profile metadata + a `Launch goose` button (shells out to `geese launch`)
- Tab state persisted across restarts at `$XDG_DATA_HOME/gander/state.toml` (override with `GANDER_STATE`)

Deferred (see [`DESIGN.md`](./DESIGN.md)):

- Embedding goose's UI inside a tab

## Build

Requires Rust 2024 edition (stable Rust 1.85+) and the COSMIC dev deps.

```bash
cargo build --release
```

## Run

```bash
cargo run --release
```

By default `gander` reads profiles from the same `$GEESE_ROOT` your `geese` CLI uses (`$XDG_DATA_HOME/geese` by default).

## Embedded webview runtime (CEF)

`gander` embeds Chromium through CEF via `iced_webview`. You must provide a Linux CEF binary distribution where the `cef`/`cef-sys` crates expect it at runtime; see the crate docs for setup details: <https://docs.rs/cef/latest/cef/>.

On Wayland, `WAYLAND_DISPLAY` is honored automatically by the CEF engine path (`--ozone-platform=wayland`). GPU compositing is intentionally disabled (`--disable-gpu` and `--in-process-gpu`) so `gander` works in Flatpak/containerized environments without extra driver passthrough.

## Why "gander"?

You have a *gander* at the *geese*. It chimes.

It's also a pun that earns its keep: `gander` is a small, opinionated COSMIC-first viewer that hosts many `geese` profiles. The lib is `geese`. The container is `gander`. Have a gander.
