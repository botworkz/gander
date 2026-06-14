# gander

A tabbed [COSMIC](https://github.com/pop-os/cosmic-epoch) viewer for [`geese`](https://github.com/phlax/geese) profiles.

## Status

Pre-alpha. Initial scaffold; not yet built against a real COSMIC desktop.

Scoped for v0:

- Tab strip with one tab per open profile (open / close / reorder)
- "Open profile" picker listing `geese` profiles, with inline "create new" affordance
- Per-tab chat UI (Leptos WASM) loaded in a wry/WebKitGTK webview
- Tab state persisted across restarts at `$XDG_DATA_HOME/gander/state.toml` (override with `GANDER_STATE`)

Deferred (see [`DESIGN.md`](./DESIGN.md)):

- ACP wiring (real `send` / `subscribe` backed by goosed)

## Requirements

- **goose ≥ 1.37.0** — gander uses the `_goose/unstable/resources/read` RPC to
  fetch MCP App HTML panels.  Earlier versions ship an incompatible response
  shape and will silently drop the panel.

## Build

Requires Rust 2024 edition (stable Rust 1.85+) and the COSMIC dev deps.

### Dev loop

**Step 1** — build the Leptos chat UI (needs [`trunk`](https://trunkrs.dev)):

```bash
cargo install trunk                   # first time only
rustup target add wasm32-unknown-unknown  # first time only
cargo xtask build-chat
```

**Step 2** — build and run gander:

```bash
cargo run
```

To enable WebKit devtools (right-click → Inspect Element inside the webview):

```bash
GANDER_DEVTOOLS=1 cargo run
```

### Release build

```bash
cargo xtask build-chat --release
cargo build --release
```

## Run

```bash
cargo run --release
```

By default `gander` reads profiles from the same `$GEESE_ROOT` your `geese` CLI uses (`$XDG_DATA_HOME/geese` by default).

## Diagnostics

All wire-level ACP traffic is logged at `debug!` level under the **`gander::wire`** target — every JSON-RPC frame sent to or received from goose, plus the `AcpEvent`s emitted from the worker. Off by default; enable with:

> **Protocol note:** the pure ACP v1 worker (`src/acp/`) is cleanly separated from
> goose-specific extensions; the `ExtHandler` trait bounds that surface, with
> `GooseExtHandler` (`src/ext/goose.rs`) being the only place `_meta.goose.*` and
> `_goose/unstable/resources/read` are referenced.

```bash
RUST_LOG=gander::wire=debug cargo run --bin gander
```

Combine with general gander debug output:

```bash
RUST_LOG=gander=debug,gander::wire=debug cargo run --bin gander
```

Log lines are structured (`field=value`) with JSON-stringified payloads so they survive copy-paste and pipe straight into `jq`. Message strings like `INIT_REQUEST`, `TOOL_CALL_UPDATE_DELTA`, and `META_PAYLOAD` are stable and greppable.

## Why "gander"?

You have a *gander* at the *geese*. It chimes.

It's also a pun that earns its keep: `gander` is a small, opinionated COSMIC-first viewer that hosts many `geese` profiles. The lib is `geese`. The container is `gander`. Have a gander.
