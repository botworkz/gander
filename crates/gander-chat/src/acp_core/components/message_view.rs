// SPDX-License-Identifier: GPL-3.0-or-later

//! Message rendering: individual bubbles and the tool-call card.
//!
//! # Virtualisation (gander#124)
//!
//! Every per-message render path that's expensive to mount —
//! `markdown::render`, `parse_tool_io`, `srcdoc` injection — is gated
//! on [`ChatMessage::visible`].  When `false` (the message is outside
//! the virtualiser's overscan window) we render a height-preserving
//! skeleton instead.  The card chrome (header text, status spine,
//! gear icon) is always rendered because it's cheap and lets the
//! user *scan* the transcript even when it's mostly skeletons —
//! titles read at-a-glance during fast scroll.

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
///
/// # Virtualisation
///
/// When `message.visible` is `false`, render a height-preserving
/// skeleton `<div>` instead of the real markdown body.  The skeleton
/// carries the message's role class so spacing / gutters match the
/// real bubble's geometry — without this, the spacer math in
/// `virtual_list::visible_window` and the actual painted height
/// diverge and the user sees the page jiggle as messages scroll in
/// and out of the window.
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
            // Bubble body shared by both speakers.  Defined once so the
            // user / assistant arms below differ only in avatar order.
            //
            // Virtualisation gate: when `visible` is false, render a
            // skeleton with the same outer chrome but no markdown
            // body.  This is the load-bearing optimisation for
            // gander#124 — `crate::markdown::render` is the dominant
            // per-message cost on long sessions.
            let bubble = view! {
                <div class="message-bubble">
                    {move || {
                        if message.visible.get() {
                            view! {
                                <div
                                    class="message-content"
                                    inner_html=move || crate::markdown::render(&message.content.get())
                                />
                            }
                            .into_any()
                        } else {
                            // Skeleton: a transparent placeholder
                            // sized by the parent flex layout.  No
                            // shimmer — animating hundreds of
                            // skeletons during a fast scroll is
                            // visual noise; we want them to read as
                            // "this is geometry, not loading".
                            view! { <div class="message-content message-content--skeleton" /> }
                                .into_any()
                        }
                    }}
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
            };

            // Avatars: gander on the assistant side (image), stock user
            // glyph on the user side.  Both sit in the same 32x32 lane
            // declared in CSS — gander leads, user trails (DOM order
            // mirrors visual order in each bubble).
            match message.role {
                Role::Assistant => view! {
                    <div class=role_class>
                        <img
                            class="message-avatar"
                            src="/assets/gander.svg"
                            alt="gander"
                        />
                        {bubble}
                    </div>
                }
                .into_any(),
                Role::User => view! {
                    <div class=role_class>
                        {bubble}
                        <span
                            class="message-avatar message-avatar--user"
                            aria-label="you"
                            role="img"
                        >
                            <Icon icon=icondata::LuUser width="18px" height="18px" />
                        </span>
                    </div>
                }
                .into_any(),
                Role::Tool => unreachable!(),
            }
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
///
/// # Virtualisation
///
/// - Header (gear, title, chevron, status spine) is **always**
///   rendered — these are scan-cheap and let the user see the
///   conversation structure at a glance during fast scroll.
/// - Body (`parse_tool_io`) is gated on `message.visible`.  The JSON
///   parse + `JSON.stringify` for input/output is the heavy bit.
/// - `expanded` lives on the message (not the component) so it
///   survives unmount/remount (gander#124).
/// - `McpAppIframe` does its own visibility-gating of `srcdoc`.
#[component]
pub fn ToolCallCard(message: ChatMessage) -> impl IntoView {
    // Promoted to `message.expanded` so toggling survives an
    // unmount/remount on scroll — without this a user who expanded a
    // card then scrolled far enough away to unmount it would find it
    // collapsed again on scroll-back.  Same toggle behaviour as
    // before, just reading from the message-level signal.
    let expanded = message.expanded;
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
            // ── body (shown when expanded AND visible) ────────────────
            //
            // Both gates needed: `expanded` is the user's intent (do
            // they want to read the I/O); `visible` is virtualiser
            // state (are we mounted at all).  Off-screen-but-expanded
            // cards still skip the parse cost; on scroll back into
            // view the body re-materialises with the user's expand
            // state preserved.
            {move || {
                (expanded.get() && message.visible.get())
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
            //
            // `visible` is threaded through so the iframe component
            // can withhold `srcdoc` injection while off-screen — see
            // the extension-layer iframe component for the gating
            // logic.  In-place when visible, placeholder when not.
            // Iframe-state retention across unmount is tracked
            // separately in gander#125.
            <McpAppIframe
                ui_html=message.ui_html
                ui_pending=message.ui_pending
                tool_call_id=message.tool_call_id
                visible=message.visible
            />
        </div>
    }
}
