// SPDX-License-Identifier: GPL-3.0-or-later

//! Event handler for goose-specific bridge events.
//!
//! Handles the `tool_resource` event, the only goose-ext variant in v1.
//! Pure-ACP events are handled in `crate::acp_core::events`.

use leptos::prelude::*;
use wasm_bindgen::JsValue;

use crate::acp_core::events::message_dest;
use crate::acp_core::types::ChatMessage;

/// Dispatch a raw JS bridge event, handling only goose-ext variants.
///
/// Currently handles `tool_resource` only.  Unknown events are silently
/// skipped — the caller is responsible for passing the event to
/// `crate::acp_core::events::handle_acp_core_bridge_event` as well.
///
/// During history replay the `tool_resource` event hydrates a card that
/// lives in `replay_buffer` rather than `messages`, so the same
/// destination-picking helper used by the ACP core handler is reused
/// here.  Without that the panel HTML would be set on a not-yet-mounted
/// signal whose owning card is still in the buffer; the visible panel
/// would never appear because `messages` is empty.
// goose-ext: sets ui_html on the matching tool-call card
pub fn handle_goose_ext_bridge_event(
    event: &JsValue,
    replaying: RwSignal<bool>,
    messages: RwSignal<Vec<ChatMessage>>,
    replay_buffer: RwSignal<Vec<ChatMessage>>,
) {
    let event_type = js_sys::Reflect::get(event, &JsValue::from_str("type"))
        .ok()
        .and_then(|v| v.as_string());

    match event_type.as_deref() {
        // ── goose-ext: pre-hydrated MCP App HTML ───────────────────────────
        // goose-ext: sets ui_html on the matching tool-call card
        Some("tool_resource") => {
            let tool_call_id = js_sys::Reflect::get(event, &JsValue::from_str("tool_call_id"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let html = js_sys::Reflect::get(event, &JsValue::from_str("html"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            let dest = message_dest(messages, replay_buffer, replaying);

            if let Some(msg) = dest
                .get_untracked()
                .iter()
                .find(|m| m.tool_call_id.get_untracked().as_deref() == Some(tool_call_id.as_str()))
                .copied()
            {
                msg.ui_pending.set(false);
                msg.ui_html.set(Some(html));
            }
        }

        _ => {
            // Ignore events not owned by this handler.
        }
    }
}
