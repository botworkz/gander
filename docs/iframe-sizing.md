# iframe sizing protocol

MCP App panels render inside a sandboxed `<iframe sandbox="allow-scripts">`
(see `crates/gander-chat/src/goose_ext/components/mcp_app_iframe.rs`).

The browser cannot auto-size an iframe to its `srcdoc` content — `height`
needs to come from the *outside*.  We use a two-part protocol:

1. **`HEIGHT_REPORTER_SCRIPT`** is appended to every iframe's `srcdoc` by
   `build_srcdoc`.  It measures `document.documentElement.scrollHeight` (and
   the equivalent body measurements, max-ed together) and posts
   ```js
   parent.postMessage(
     { type: "gander.iframe.height", id: window.name, height: N },
     "*"
   );
   ```
   on `load`, on `resize`, and whenever `ResizeObserver` fires on `<html>`.
   The script is guarded with `window.__ganderHeightReporter` so a panel that
   embeds its own copy doesn't end up with two observers.

2. **The parent listener** lives in `crates/gander-chat/index.html` (and the
   compiled `dist/index.html`).  It listens for `message` events with
   `data.type === "gander.iframe.height"` and resizes the matching
   `<iframe class="tool-call-iframe" name="<id>">` via `style.height = h+"px"`.

The iframe's `name` attribute is set to its tool-call id (the same id the
parent uses to find the card), so each panel sizes its own iframe and the
parent listener doesn't need to track ownership.

## CSS bounds

```css
.tool-call-iframe {
  width: 100%;
  min-height: 200px;
  max-height: 80vh;
}
```

* `min-height: 200px` — small panels keep visual presence; a one-line
  "i Notice" doesn't read as an empty card.  This is the "rather-too-big-
  than-too-small" default from the issue discussion.
* `max-height: 80vh` — runaway panels can't eat the whole viewport.
* `min-height` on `.tool-call-iframe-pending` matches the iframe so the card
  doesn't shift vertically when the placeholder is swapped for the iframe.

The parent listener also floors incoming heights at 200px so a panel that
transiently reports `0` during a re-layout doesn't make the iframe collapse.

## Why postMessage and not ResizeObserver on the iframe itself

`new ResizeObserver(...).observe(iframe)` only fires when the *iframe
element* resizes — i.e. when CSS layout assigns it a different size, not
when its content grows.  The browser has no built-in way to observe content
height across a sandboxed cross-origin boundary; `postMessage` is the
standard escape hatch.

## Sandbox

`sandbox="allow-scripts"` is sufficient for `postMessage` to the parent.
`allow-same-origin` is deliberately not granted — we don't want panels
reading cookies or storage.  Cross-origin `postMessage` (with `"*"` as the
target origin) works fine across this boundary.

## Dev mock

`crates/gander-chat/index.html` contains a development mock for
`window.gander` (used by `trunk serve`).  The mock fires a `tool_resource`
event for `tc-mock-2` shortly after the corresponding `tool_call`
completes, so the sizing protocol can be exercised against a panel that
emits real heights without needing a live goose.
