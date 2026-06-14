// SPDX-License-Identifier: GPL-3.0-or-later

//! Sandboxed iframe for MCP App HTML panels.
//!
//! Rendered inside a `ToolCallCard` when the host emits a `tool_resource`
//! event that carries pre-hydrated HTML for the tool call.

use leptos::prelude::*;

/// Sandboxed iframe that displays MCP App HTML for a tool-call card.
///
/// Renders nothing when `ui_html` is `None`.
///
/// The sandbox is intentionally minimal: `allow-scripts` only.
/// `allow-same-origin`, `allow-forms`, and `allow-top-navigation` are
/// deliberately excluded to prevent the iframe content from accessing
/// cookies/storage, submitting forms, or escaping the sandbox.
// goose-ext: rendered when the host emits tool_resource for this call
#[component]
pub fn McpAppIframe(ui_html: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        {move || {
            ui_html.get().map(|html| {
                view! {
                    <iframe
                        class="tool-call-iframe"
                        sandbox="allow-scripts"
                        srcdoc=html
                    />
                }
            })
        }}
    }
}
