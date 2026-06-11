# gander design

`gander` is a tabbed viewer for `geese` profiles, written against `libcosmic`. It depends on `geese` as a Rust library — all profile operations (list, create, copy, lock, delete, resolve `GOOSE_PATH_ROOT`) are direct function calls, not shelling out to the `geese` binary.

## Goal

A single COSMIC window with a tab per `goose` instance, where the tab body is the *actual* goose UI rendered inline. "Have a gander at the geese."

Wayland does not let one process reparent another process's toplevel surface, so we cannot tab a separate Electron window into gander. The path to the goal is therefore to drop the Electron shell entirely and host goose's React UI inside a webview embedded in each iced tab — option **(1)** below.

That's the destination. The current scaffold is **option (2)** — gander is a tabbed launcher whose tab bodies are profile panes with a "Launch goose" button that spawns a separate Electron goose. Option (2) earns its keep as a v0 because it shakes out the libcosmic plumbing, the `geese` library integration, and the persistent-state contract without committing to webview embedding in the same commit. It is explicitly a holding pattern.

## Scope (v0, current scaffold)

In:

- COSMIC-native window with a horizontal tab bar
- One tab per *open* profile (a strict subset of all known profiles)
- Open / close tabs
- "+ tab" opens an in-window picker listing all known `geese` profiles with an inline "create new profile" affordance
- Per-tab placeholder body showing profile metadata, resolved `GOOSE_PATH_ROOT`, and a `Launch goose` button that spawns goose detached via `geese::Profile::command` (no shell-out to the `geese` binary)
- Persistent UI state: open tabs, tab order, active tab

Out for v0:

- Embedding goose's UI inside a tab (see *Path to the goal* below)
- Drag-to-reorder tabs
- Keyboard shortcuts
- Profile editing beyond create
- Multi-window
- Non-Linux

## Storage

```
$XDG_DATA_HOME/gander/
└── state.toml      # tab order, active tab
```

Override the location with `GANDER_STATE`. Profile data itself lives under `$GEESE_ROOT` and is owned by `geese`; `gander` never writes there.

### state.toml v0

```toml
version = 0
active = "work-stable"

[[tab]]
name = "work-stable"

[[tab]]
name = "scratch"
```

If a profile listed in `state.toml` no longer exists when `gander` starts, the tab is silently dropped.

## Architecture

Standard [`cosmic::Application`](https://github.com/pop-os/libcosmic) shape:

- `AppModel` — owns `geese::Storage`, the open-tab list, persistent state handle
- `Message` — `OpenTab(name)`, `CloseTab(entity)`, `ActivateTab(entity)`, `LaunchGoose(name)`, `CreateProfile`, `NewProfileNameChanged`, `RefreshProfiles`, `UpdateConfig`, …
- `view()` — header bar with a `New tab` button → opens an in-window picker page; tab strip via `widget::tab_bar::horizontal`; content area is whatever the active tab renders
- `subscription()` — watches the cosmic config; eventually also `$GEESE_ROOT` for external changes

The tab content area is deliberately a single `Element` produced by the active tab. That's the seam: swapping the placeholder body for an embedded webview is a localised change in `tab.rs` rather than a cross-cutting refactor.

## Path to the goal

### Option (1) — Webview-embedded goose UI per tab *(target)*

Stop running goose as a separate top-level Electron window. Instead, per tab:

- spawn one `goosed` (goose's backend) with the tab's `GOOSE_PATH_ROOT`
- load goose's React UI bundle in an [`iced_webview`](https://github.com/LegitCamper/iced_webview)-backed webview hosted inside the iced tab content area, pointed at that `goosed`
- one Chromium per gander (not per profile), real tabbed UX, no compositor games

Required from goose: a way to load its UI standalone (no Electron `ipcRenderer`, no Electron-specific main process). That's its own piece of work and almost certainly means upstream changes to goose, or a long-lived fork. Tracking issue / PR for that lives in the `goose` repo, not here.

Required from gander: an iced-native webview integration. Non-trivial but well-trodden territory; drop to raw [`wry`](https://github.com/tauri-apps/wry) only if the wrapper proves to be a real blocker on COSMIC/Wayland.

### Option (2) — Compositor-grouped Electron goose *(current scaffold)*

Spawn a real Electron goose per profile, let the compositor group the windows. This is what the scaffold does today. Multiple concurrent instances need goose to be patched so its windows aren't all identical to the compositor — see `phlax/goose#1` (`GOOSE_APP_ID` + `GOOSE_INSTANCE_LABEL`), which is fork-local for now and *required* for this option to be usable as more than a single-window demo.

This is also where `geese`'s stacker PR (`phlax/geese#2`) plays — the compositor-side grouping of goose top-levels into a visual flock.

### Option (3) — Hybrid

Some tabs are native iced views (profile editor, logs, settings); the "current goose conversation" tab opens goose in a top-level. Useful as a stepping stone if (1) is slow to land — the iced bits we'd build for the lightweight tabs are reusable when (1) arrives.

## Naming

The project is named `gander` because:

- you have a *gander* at the *geese*
- it sits well alongside `goose` and `geese`
- "take a gander" carries the looking-at-stuff sense more strongly than the gendered-goose sense, which suits a viewer

The "post-modern goose" framing in the README is a lampshade, not a load-bearing claim.
