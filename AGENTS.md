# AGENTS.md

## What gander is

A native COSMIC client for goose, communicating via the Agent Client Protocol.
Not a wrapper for the Electron app, not a launcher for separate goose windows.
See DESIGN.md for the architectural shape.

## Conventions

- YAML: block sequences at the same indent as their parent key, never indented
  relative to it
- Comments in code explain *why*, not *what*
- POC stage; prefer obviously correct over clever
- One thing per PR; don't fold unrelated changes together
- No condescending agent-speak — write like you would to a colleague

## Tech choices already made (don't relitigate)

- iced/libcosmic for the shell (`crates/gander`)
- Leptos for the chat UI (`crates/gander-chat`) — chosen over Yew/Dioxus for
  fine-grained reactivity on streaming updates
- wry + WebKitGTK for the per-tab webview, accepting XWayland-only v0 (see
  DESIGN.md "Known constraint")
- ACP v1 over stdio (migrate to ACP+ over HTTP/WS when SDK ships it)
- Per-tab process — one goose child per tab, owned by `supervisor::Supervisor`
- "Out of mem out of mind" — close tab → kill goose → drop webview

## Decisions deliberately deferred

- Embedding the gander-chat bundle in the gander binary (currently loaded
  from disk during dev; embed via `include_dir!` when stable)
- ACP+ HTTP/WS transport (stdio works today, SDK will gain HTTP later)
- Native Wayland support (waiting on upstream wry)
- Settings drawer beyond "goose binary path" (only one setting today, no need)
- Multi-window (single main window, tabs only)
- Cross-platform (Linux-only by design; iced/libcosmic mean it should run on
  any Linux desktop in principle, but COSMIC is the test target)

## What NOT to do

- Don't write an "embed the existing Electron React UI" path. That's
  explicitly rejected; gander is a native ACP client.
- Don't reach for CEF / Servo / Blitz. Already evaluated, rejected.
- Don't add a "platform abstraction layer" — direct deps on iced, wry,
  leptos, used directly.
- Don't fork goose to make it embedabble. Talk to it via ACP.
