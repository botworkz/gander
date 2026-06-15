// SPDX-License-Identifier: GPL-3.0-or-later

//! Sandboxed iframe for MCP App HTML panels.
//!
//! Rendered inside a `ToolCallCard` when the host emits a `tool_resource`
//! event that carries pre-hydrated HTML for the tool call.

use leptos::prelude::*;

/// Sandboxed iframe that displays MCP App HTML for a tool-call card.
///
/// Renders nothing when both `ui_html` is `None` and `ui_pending` is `false`.
/// Shows a loading placeholder while `ui_pending` is `true` (resource fetch in
/// flight) and the real iframe once `ui_html` becomes `Some`.
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
) -> impl IntoView {
    view! {
        {move || {
            match (ui_pending.get(), ui_html.get()) {
                (_, Some(html)) => Some(
                    view! {
                        <iframe
                            class="tool-call-iframe"
                            sandbox="allow-scripts"
                            srcdoc=html
                        />
                    }
                    .into_any(),
                ),
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
