// SPDX-License-Identifier: GPL-3.0-or-later

//! Event handler for pure-ACP bridge events.
//!
//! Handles all `handle_bridge_event` match arms that correspond to
//! standard ACP protocol events.  The extension-specific `tool_resource`
//! variant is handled in the extension events module.

use js_sys;
use leptos::prelude::*;
use wasm_bindgen::JsValue;

use crate::acp_core::types::{ChatMessage, SessionEntry, DEFAULT_SESSION_LABEL};

/// Consume and return the next message ID.
pub(crate) fn take_id(next_id: RwSignal<u32>) -> u32 {
    let id = next_id.get_untracked();
    next_id.set(id + 1);
    id
}

/// Dispatch a raw JS bridge event to the appropriate signal update.
///
/// Handles only the pure-ACP event variants.  Unknown events and the
/// `tool_resource` variant are silently skipped here; the caller also
/// passes the event to the extension event handler.
#[allow(clippy::too_many_arguments)]
pub fn handle_acp_core_bridge_event(
    event: &JsValue,
    in_flight: RwSignal<Option<ChatMessage>>,
    sending: RwSignal<bool>,
    replaying: RwSignal<bool>,
    messages: RwSignal<Vec<ChatMessage>>,
    next_id: RwSignal<u32>,
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
    footer_cwd: RwSignal<Option<String>>,
    footer_model: RwSignal<Option<String>>,
    footer_tool_count: RwSignal<Option<u32>>,
) {
    let event_type = js_sys::Reflect::get(event, &JsValue::from_str("type"))
        .ok()
        .and_then(|v| v.as_string());

    match event_type.as_deref() {
        // ── agent text chunk (live or history replay) ──────────────────────
        Some("agent_text") => {
            let chunk = js_sys::Reflect::get(event, &JsValue::from_str("content"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            // Re-use the existing in-flight bubble or start a new one.
            let msg = match in_flight.get_untracked() {
                Some(m) => m,
                None => {
                    let m = ChatMessage::new_assistant(take_id(next_id));
                    messages.update(|v| v.push(m));
                    in_flight.set(Some(m));
                    m
                }
            };
            msg.content.update(|c| c.push_str(&chunk));
        }

        // ── user text chunk (history replay only) ──────────────────────────
        Some("user_text") => {
            let text = js_sys::Reflect::get(event, &JsValue::from_str("content"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            // A user message from history is always complete. If there is an
            // in-flight agent bubble, close it first to keep ordering correct.
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
                in_flight.set(None);
            }

            let m = ChatMessage::new_user(take_id(next_id), &text);
            messages.update(|v| v.push(m));
        }

        // ── tool call (merged snapshot) ────────────────────────────────────
        Some("tool_call") => {
            let call_json = js_sys::Reflect::get(event, &JsValue::from_str("call"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            // Parse the JSON string to extract tool_call_id and status.
            let parsed = js_sys::JSON::parse(&call_json).ok();
            let tool_call_id = parsed
                .as_ref()
                .and_then(|obj| js_sys::Reflect::get(obj, &JsValue::from_str("toolCallId")).ok())
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let status = parsed
                .as_ref()
                .and_then(|obj| js_sys::Reflect::get(obj, &JsValue::from_str("status")).ok())
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let is_done = crate::acp_core::components::message_view::is_terminal_status(&status);

            // A completed tool call with rawOutput.resourceUri means a UI
            // resource fetch is in flight.  Set ui_pending so the card can
            // show a loading placeholder immediately — before tool_resource
            // arrives — preventing the visible pop-in gap described in #100.
            let has_resource_uri = parsed
                .as_ref()
                .and_then(|obj| js_sys::Reflect::get(obj, &JsValue::from_str("rawOutput")).ok())
                .and_then(|ro| js_sys::Reflect::get(&ro, &JsValue::from_str("resourceUri")).ok())
                .map(|v| !v.is_null() && !v.is_undefined())
                .unwrap_or(false);
            let ui_pending = is_done && has_resource_uri;

            // Find an existing card for this tool call id, or create one.
            let existing = messages
                .get_untracked()
                .iter()
                .find(|m| m.tool_call_id.get_untracked().as_deref() == Some(tool_call_id.as_str()))
                .copied();

            match existing {
                Some(msg) => {
                    msg.content.set(call_json);
                    msg.streaming.set(!is_done);
                    if ui_pending {
                        msg.ui_pending.set(true);
                    }
                }
                None => {
                    let m = ChatMessage::new_tool(take_id(next_id), tool_call_id, call_json);
                    m.streaming.set(!is_done);
                    if ui_pending {
                        m.ui_pending.set(true);
                    }
                    messages.update(|v| v.push(m));
                }
            }
        }

        // ── session load start: clear UI, enter replay mode ────────────────
        Some("session_load_start") => {
            messages.set(Vec::new());
            in_flight.set(None);
            sending.set(false);
            replaying.set(true);
        }

        // ── session load end: leave replay mode ────────────────────────────
        Some("session_load_end") => {
            // Close any lingering in-flight bubble from the last replay turn.
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
                in_flight.set(None);
            }
            replaying.set(false);
        }

        Some("done") => {
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
        }

        Some("error") => {
            let message = js_sys::Reflect::get(event, &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "Unknown error".to_string());

            if let Some(msg) = in_flight.get_untracked() {
                msg.error.set(Some(message));
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
            replaying.set(false);
        }

        Some("session_list") => {
            let raw_sessions = js_sys::Reflect::get(event, &JsValue::from_str("sessions"))
                .ok()
                .filter(|v| v.is_array())
                .map(|v| js_sys::Array::from(&v))
                .unwrap_or_default();

            let mut entries: Vec<SessionEntry> = Vec::new();
            for i in 0..raw_sessions.length() {
                let item = raw_sessions.get(i);
                let id = js_sys::Reflect::get(&item, &JsValue::from_str("id"))
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                let label = js_sys::Reflect::get(&item, &JsValue::from_str("label"))
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_else(|| DEFAULT_SESSION_LABEL.to_string());
                let last_active = js_sys::Reflect::get(&item, &JsValue::from_str("last_active"))
                    .ok()
                    .and_then(|v| v.as_string());
                if !id.is_empty() {
                    entries.push(SessionEntry {
                        id,
                        label,
                        last_active,
                    });
                }
            }
            sessions.set(entries);
        }

        Some("session_active") => {
            let id = js_sys::Reflect::get(event, &JsValue::from_str("id"))
                .ok()
                .and_then(|v| v.as_string());
            active_session_id.set(id);
            // Clear chat messages when a new session is created (via
            // `session_new` command). History replay uses `session_load_start`
            // instead and is handled separately above.
            messages.set(Vec::new());
            in_flight.set(None);
            sending.set(false);
        }

        // ── session metadata for the footer bar ───────────────────────────
        Some("session_info") => {
            let cwd = js_sys::Reflect::get(event, &JsValue::from_str("cwd"))
                .ok()
                .and_then(|v| v.as_string());
            let model = js_sys::Reflect::get(event, &JsValue::from_str("model"))
                .ok()
                .and_then(|v| v.as_string());
            // tool_count may be null (not yet supported) or a number.
            let tool_count = js_sys::Reflect::get(event, &JsValue::from_str("tool_count"))
                .ok()
                .and_then(|v| v.as_f64())
                .map(|n| n as u32);

            if let Some(c) = cwd {
                footer_cwd.set(Some(c));
            }
            if let Some(m) = model {
                footer_model.set(Some(m));
            }
            footer_tool_count.set(tool_count);
        }

        _ => {
            // Ignore unrecognised events (including extension variants handled
            // separately) so future protocol extensions are forwards-compatible.
        }
    }
}
