# gander

A tabbed [COSMIC](https://github.com/pop-os/cosmic-epoch) viewer for [`geese`](https://github.com/phlax/geese) profiles.

> *gander, n. — a post-modern goose; prefers the company of the flock to the headship of it.*

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

## Why "gander"?

You have a *gander* at the *geese*. It chimes.

It's also a pun that earns its keep: `gander` is a small, opinionated COSMIC-first viewer that hosts many `geese` profiles. The lib is `geese`. The container is `gander`. Have a gander.
