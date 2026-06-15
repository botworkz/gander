// SPDX-License-Identifier: GPL-3.0-or-later

//! Sandboxed iframe for MCP App HTML panels.
//!
//! Rendered inside a `ToolCallCard` when the host emits a `tool_resource`
//! event that carries pre-hydrated HTML for the tool call.

use leptos::prelude::*;

/// Helper script appended to every iframe `srcdoc` so the panel can report
/// its content height back to the parent document.
///
/// The parent listens with the matching `gander.iframe.height` handler
/// installed in `index.html` and resizes the iframe by `name`.  This is the
/// host side of the protocol described in `docs/iframe-sizing.md`.
///
/// Defensive: bails out silently on any error and short-circuits if a prior
/// copy already installed itself (panels that embed their own copy of the
/// helper get a single observer, not two).
///
/// `allow-scripts` is sufficient for `postMessage` to the parent — no
/// `allow-same-origin` required.  The message is cross-origin by design and
/// carries only the iframe `name` (the tool-call id) so the parent can route
/// it to the right `<iframe>`.
const HEIGHT_REPORTER_SCRIPT: &str = r#"<script>
(function(){
  if (window.__ganderHeightReporter) return;
  window.__ganderHeightReporter = true;
  function measure(){
    var d = document.documentElement;
    var b = document.body;
    return Math.max(
      d ? d.scrollHeight : 0,
      d ? d.offsetHeight : 0,
      b ? b.scrollHeight : 0,
      b ? b.offsetHeight : 0
    );
  }
  var last = -1;
  function post(){
    try {
      var h = measure();
      if (h === last) return;
      last = h;
      parent.postMessage({ type: "gander.iframe.height", id: window.name, height: h }, "*");
    } catch (e) {}
  }
  function start(){
    post();
    if ("ResizeObserver" in window && document.documentElement) {
      try { new ResizeObserver(post).observe(document.documentElement); } catch (e) {}
    }
    window.addEventListener("load", post);
    window.addEventListener("resize", post);
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
</script>"#;

/// Build the iframe `srcdoc` for a tool call.
///
/// Appends [`HEIGHT_REPORTER_SCRIPT`] so the parent can size the iframe to
/// its content.  Panels that omit a `</body>` close tag still work — the
/// browser is lenient about late `<script>` tags in `srcdoc`.
fn build_srcdoc(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + HEIGHT_REPORTER_SCRIPT.len());
    out.push_str(html);
    out.push_str(HEIGHT_REPORTER_SCRIPT);
    out
}

/// Sandboxed iframe that displays MCP App HTML for a tool-call card.
///
/// Renders nothing when both `ui_html` is `None` and `ui_pending` is `false`.
/// Shows a loading placeholder while `ui_pending` is `true` (resource fetch in
/// flight) and the real iframe once `ui_html` becomes `Some`.
///
/// The iframe is sized by the parent: `index.html` sets an initial
/// `min-height` and listens for `gander.iframe.height` messages from the
/// helper script appended to the `srcdoc`, then resizes the iframe to match
/// its content (clamped to `max-height: 80vh`).  Panels that never post a
/// height keep the initial `min-height`, satisfying the
/// "rather-too-big-than-too-small" default.
///
/// The sandbox is intentionally minimal: `allow-scripts` only.
/// `allow-same-origin`, `allow-forms`, and `allow-top-navigation` are
/// deliberately excluded to prevent the iframe content from accessing
/// cookies/storage, submitting forms, or escaping the sandbox.
// goose-ext: rendered when the host emits tool_resource for this call
#[component]
pub fn McpAppIframe(
    ui_html: RwSignal<Option<String>>,
    ui_pending: RwSignal<bool>,
    /// Tool-call id, used as the iframe `name` so the parent's
    /// `gander.iframe.height` listener can route height updates back to the
    /// right `<iframe>` element.
    tool_call_id: RwSignal<Option<String>>,
) -> impl IntoView {
    view! {
        {move || {
            match (ui_pending.get(), ui_html.get()) {
                (_, Some(html)) => {
                    let srcdoc = build_srcdoc(&html);
                    let name = tool_call_id.get().unwrap_or_default();
                    Some(
                        view! {
                            <iframe
                                class="tool-call-iframe"
                                sandbox="allow-scripts"
                                name=name
                                srcdoc=srcdoc
                            />
                        }
                        .into_any(),
                    )
                }
                (true, None) => Some(
                    view! {
                        <div class="tool-call-iframe-pending">
                            <span class="tool-call-iframe-pending-label">"Loading panel…"</span>
                        </div>
                    }
                    .into_any(),
                ),
                (false, None) => None,
            }
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_srcdoc_appends_height_reporter() {
        let out = build_srcdoc("<p>hi</p>");
        assert!(out.starts_with("<p>hi</p>"));
        assert!(
            out.contains("gander.iframe.height"),
            "srcdoc must carry the height-reporter postMessage tag",
        );
        assert!(
            out.contains("__ganderHeightReporter"),
            "srcdoc must carry the dedup guard",
        );
    }

    #[test]
    fn build_srcdoc_is_idempotent_in_length_growth() {
        // The helper is appended once; calling with empty html should still
        // produce a runnable script tag (browser will execute lone <script>).
        let out = build_srcdoc("");
        assert!(out.starts_with("<script>"));
        assert!(out.ends_with("</script>"));
    }
}
