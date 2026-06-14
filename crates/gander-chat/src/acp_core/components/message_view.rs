// SPDX-License-Identifier: GPL-3.0-or-later

//! Message rendering: individual bubbles and the tool-call card.

use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::JsValue;

use crate::acp_core::types::{ChatMessage, Role};
// McpAppIframe is in the extension layer; re-exported from the crate root so
// that this module stays free of extension-layer path references.
use crate::McpAppIframe;

/// Return `true` for ACP `ToolCallStatus` values that indicate the call
/// is finished (`"completed"` or `"failed"`).
pub fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

/// Extract the tool title from a serialised `ToolCall` JSON string.
fn parse_tool_title(json: &str) -> String {
    js_sys::JSON::parse(json)
        .ok()
        .and_then(|obj| js_sys::Reflect::get(&obj, &JsValue::from_str("title")).ok())
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "(tool)".to_string())
}

/// Return the CSS state class suffix for a tool-call card.
///
/// Reads only the JSON `status` field — the `streaming` signal is
/// intentionally ignored here so that history replay of a snapshot
/// saved mid-flight reflects its actual saved state rather than
/// throbbing indefinitely.
fn tool_status_class(json: &str) -> &'static str {
    let status = js_sys::JSON::parse(json)
        .ok()
        .and_then(|obj| js_sys::Reflect::get(&obj, &JsValue::from_str("status")).ok())
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    match status.as_str() {
        "in_progress" => "running",
        "completed" => "success",
        "failed" => "failure",
        _ => "pending",
    }
}

/// Extract pretty-printed input and output from a serialised `ToolCall` JSON string.
///
/// Returns `(input, output)` where each is `None` when absent.
fn parse_tool_io(json: &str) -> (Option<String>, Option<String>) {
    let Ok(obj) = js_sys::JSON::parse(json) else {
        return (None, None);
    };
    let pretty = |key: &str| -> Option<String> {
        js_sys::Reflect::get(&obj, &JsValue::from_str(key))
            .ok()
            .filter(|v| !v.is_null() && !v.is_undefined())
            .and_then(|v| {
                js_sys::JSON::stringify_with_replacer_and_space(
                    &v,
                    &JsValue::NULL,
                    &JsValue::from_f64(2.0),
                )
                .ok()
                .and_then(|s| s.as_string())
            })
    };
    (pretty("rawInput"), pretty("rawOutput"))
}

/// A single message bubble (user, assistant, or tool).
///
/// Tool messages are rendered as a collapsible card via [`ToolCallCard`].
/// All other messages render their content via the markdown renderer.
#[component]
pub fn MessageView(message: ChatMessage) -> impl IntoView {
    match message.role {
        Role::Tool => view! { <ToolCallCard message /> }.into_any(),
        _ => {
            let role_class = match message.role {
                Role::User => "message message--user",
                Role::Assistant => "message message--assistant",
                Role::Tool => unreachable!(),
            };
            view! {
                <div class=role_class>
                    // Avatar gutter — only present for assistant messages.
                    {matches!(message.role, Role::Assistant).then(|| {
                        view! {
                            <img
                                class="message-avatar"
                                src="/assets/gander.png"
                                alt="gander"
                            />
                        }
                    })}
                    // Bubble body: content, streaming cursor, and error notice.
                    <div class="message-bubble">
                        // Reactive inner HTML: re-evaluates only when `content` changes.
                        <div
                            class="message-content"
                            inner_html=move || crate::markdown::render(&message.content.get())
                        />
                        // Blinking cursor while the host is still streaming tokens.
                        {move || {
                            message
                                .streaming
                                .get()
                                .then(|| view! { <span class="streaming-cursor">"▋"</span> })
                        }}
                        // Error notice, shown only on failure.
                        {move || {
                            message
                                .error
                                .get()
                                .map(|e| view! { <div class="error-notice">{e}</div> })
                        }}
                    </div>
                </div>
            }
            .into_any()
        }
    }
}

/// Collapsible card for a single tool-call / tool-result pair.
///
/// The card header always shows the tool title and a status badge.  The body
/// (raw input and raw output) is hidden by default and revealed when the user
/// clicks the header.
///
/// `message.content` holds the full ACP `ToolCall` JSON snapshot; it is
/// re-evaluated whenever the host emits an update for this tool call id.
#[component]
pub fn ToolCallCard(message: ChatMessage) -> impl IntoView {
    // Local signal for the collapsed/expanded state of this card.
    let expanded: RwSignal<bool> = RwSignal::new(false);
    let toggle = move |_| expanded.update(|e| *e = !*e);

    view! {
        <div class="message message--tool tool-call-card">
            // ── state-coloured spine (absolute positioned, full card height) ──
            <div class=move || {
                let json = message.content.get();
                format!("tool-call-spine tool-call-spine--{}", tool_status_class(&json))
            } />
            // ── header ───────────────────────────────────────────────────
            <button class="tool-call-header" on:click=toggle>
                <span class=move || {
                    let json = message.content.get();
                    format!("tool-call-gear tool-call-gear--{}", tool_status_class(&json))
                }>
                    <Icon icon=icondata::LuCog width="14px" height="14px" />
                </span>
                <span class="tool-call-title">
                    {move || {
                        let json = message.content.get();
                        parse_tool_title(&json)
                    }}
                </span>
                <span class=move || {
                    if expanded.get() {
                        "tool-call-chevron tool-call-chevron--open"
                    } else {
                        "tool-call-chevron"
                    }
                }>
                    <Icon
                        icon=icondata::LuChevronRight
                        width="14px"
                        height="14px"
                    />
                </span>
            </button>
            // ── body (shown when expanded) ────────────────────────────
            {move || {
                expanded
                    .get()
                    .then(|| {
                        let json = message.content.get();
                        let (input_str, output_str) = parse_tool_io(&json);
                        view! {
                            <div class="tool-call-body">
                                {input_str
                                    .map(|inp| {
                                        view! {
                                            <div class="tool-call-section">
                                                <div class="tool-call-section-label">"Input"</div>
                                                <pre class="tool-call-pre">{inp}</pre>
                                            </div>
                                        }
                                    })}
                                {output_str
                                    .map(|out| {
                                        view! {
                                            <div class="tool-call-section">
                                                <div class="tool-call-section-label">"Output"</div>
                                                <pre class="tool-call-pre">{out}</pre>
                                            </div>
                                        }
                                    })}
                            </div>
                        }
                    })
            }}
            // ── MCP App HTML panel (sandboxed iframe, extension layer) ──────
            <McpAppIframe ui_html=message.ui_html />
        </div>
    }
}
