// SPDX-License-Identifier: GPL-3.0-or-later

//! Event handler for pure-ACP bridge events.
//!
//! Handles all `handle_bridge_event` match arms that correspond to
//! standard ACP protocol events.  The extension-specific `tool_resource`
//! variant is handled in the extension events module.
//!
//! # Replay buffering (gander#119)
//!
//! During history replay (`replaying.get_untracked() == true`) every
//! message append routes through `replay_buffer` instead of `messages`.
//! On `session_load_end` the buffer is swapped wholesale into `messages`
//! in a single update so the keyed `<For>` performs one diff for the
//! whole transcript rather than one per turn.  Without this, opening a
//! session with hundreds of turns thrashes the reconciler for several
//! seconds and the user sees text crawling top-down toward "now".
//!
//! The `in_flight` tracking, tool-call splicing for panel-bearing cards
//! (gander#107), and every other piece of bookkeeping operate on the
//! current destination — they don't care which `Vec` it is.

use js_sys;
use leptos::prelude::*;
use wasm_bindgen::JsValue;

use crate::acp_core::types::{AllSessionsState, ChatMessage, SessionEntry, DEFAULT_SESSION_LABEL};

/// Consume and return the next message ID.
pub(crate) fn take_id(next_id: RwSignal<u32>) -> u32 {
    let id = next_id.get_untracked();
    next_id.set(id + 1);
    id
}

/// Pick the message destination signal for the current phase.
///
/// During history replay (`replaying == true`) writes are accumulated in
/// `replay_buffer` so the final swap-into-`messages` is a single keyed
/// diff (see module docs).  Otherwise writes go straight to `messages`
/// because we want live tokens to land on screen immediately.
///
/// Returns a `Copy` `RwSignal`, so it can be moved into any closure that
/// follows without borrowing concerns.
#[inline]
pub(crate) fn message_dest(
    messages: RwSignal<Vec<ChatMessage>>,
    replay_buffer: RwSignal<Vec<ChatMessage>>,
    replaying: RwSignal<bool>,
) -> RwSignal<Vec<ChatMessage>> {
    if replaying.get_untracked() {
        replay_buffer
    } else {
        messages
    }
}

/// Parse the `sessions` array from a `session_list` / `all_sessions_list`
/// event into `Vec<SessionEntry>`.
///
/// Tolerant of missing / malformed fields: any entry without a non-empty
/// `id` is silently skipped (an empty id can't drive `session_select`),
/// `label` falls back to `DEFAULT_SESSION_LABEL`, and `last_active` is
/// optional.  Both bridge events use the same `ListedSession` schema so
/// they share this parser.
fn parse_session_entries(event: &JsValue) -> Vec<SessionEntry> {
    let raw_sessions = js_sys::Reflect::get(event, &JsValue::from_str("sessions"))
        .ok()
        .filter(|v| v.is_array())
        .map(|v| js_sys::Array::from(&v))
        .unwrap_or_default();

    let mut entries: Vec<SessionEntry> = Vec::with_capacity(raw_sessions.length() as usize);
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
    entries
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
    replay_buffer: RwSignal<Vec<ChatMessage>>,
    next_id: RwSignal<u32>,
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
    footer_cwd: RwSignal<Option<String>>,
    footer_model: RwSignal<Option<String>>,
    footer_tool_count: RwSignal<Option<u32>>,
    all_sessions: RwSignal<AllSessionsState>,
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

            let dest = message_dest(messages, replay_buffer, replaying);

            // Re-use the existing in-flight bubble or start a new one.
            let msg = match in_flight.get_untracked() {
                Some(m) => m,
                None => {
                    let m = ChatMessage::new_assistant(take_id(next_id));
                    dest.update(|v| v.push(m));
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

            let dest = message_dest(messages, replay_buffer, replaying);
            let m = ChatMessage::new_user(take_id(next_id), &text);
            dest.update(|v| v.push(m));
        }

        // ── tool call (merged snapshot) ────────────────────────────────────
        Some("tool_call") => {
            // Parse the snapshot.
            let call_json = js_sys::Reflect::get(event, &JsValue::from_str("call"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
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

            // Close any in-flight assistant bubble before touching the tool
            // card.
            //
            // A typical tool turn streams as
            //   [agent_text*, tool_call, tool_resource, agent_text*]
            // The pre-tool `agent_text` chunks accumulate into one bubble;
            // the post-tool chunks should land in a *new* bubble so the
            // visible order is `[pre-text, card, post-text]` rather than
            // `[pre-text + post-text concatenated, card]`.  This mirrors
            // the same close-on-boundary behaviour already in `user_text`.
            //
            // Once `in_flight` is `None`, the next `agent_text` arm creates
            // a fresh bubble and pushes it onto `messages`, landing after
            // the card we add below.
            let pre_tool_bubble = in_flight.get_untracked();
            if let Some(bubble) = pre_tool_bubble {
                bubble.streaming.set(false);
                in_flight.set(None);
            }

            let dest = message_dest(messages, replay_buffer, replaying);

            // Find an existing card for this tool call id (status update
            // path) or signal we need to create a new one.
            let existing = dest
                .get_untracked()
                .iter()
                .find(|m| m.tool_call_id.get_untracked().as_deref() == Some(tool_call_id.as_str()))
                .copied();

            // Existing-card field updates are independent of Vec layout —
            // do them up-front so the Vec mutation below is the single
            // point that may re-order the messages list.
            if let Some(msg) = existing {
                msg.content.set(call_json.clone());
                msg.streaming.set(!is_done);
                if ui_pending {
                    msg.ui_pending.set(true);
                }
            }

            // For panel-bearing tools, splice the pre-tool prose bubble
            // out of `messages` and re-append it after the card.
            //
            // Why panel-only: when the tool has a UI panel the LLM
            // typically commits to prose like "There you go — the panel is
            // mounted above…" *before* firing the tool, so the bubble
            // naturally lands at `messages.last()` by the time `tool_call`
            // arrives.  Without splicing the user sees:
            //
            //   [user: question]
            //   [assistant: There you go — the panel is above…]
            //   [tool card with iframe panel]
            //
            // — which contradicts the prose's "above".  Re-appending after
            // the card gives:
            //
            //   [user: question]
            //   [tool card with iframe panel]
            //   [assistant: There you go — the panel is above…]
            //
            // For action tools with no panel, the natural
            // `[pre-text, card, post-text]` close-on-boundary layout (above)
            // is correct as-is, so splicing would only create the wart of
            // pre-text-after-card.  Hence the `ui_pending` gate.
            //
            // Symptom this fixes: gander#100 follow-up "panel still
            // appears after dialog" — the iframe pop-in gap was closed by
            // the ui_pending placeholder in #106, but the bubble sitting
            // above the card was a separate ordering bug.
            let bubble_to_reorder = if ui_pending { pre_tool_bubble } else { None };

            let new_card = if existing.is_none() {
                let m = ChatMessage::new_tool(take_id(next_id), tool_call_id, call_json);
                m.streaming.set(!is_done);
                if ui_pending {
                    m.ui_pending.set(true);
                }
                Some(m)
            } else {
                None
            };

            dest.update(|v| {
                if let Some(bubble) = bubble_to_reorder {
                    if let Some(idx) = v.iter().position(|m| m.id == bubble.id) {
                        v.remove(idx);
                    }
                }
                if let Some(card) = new_card {
                    v.push(card);
                }
                if let Some(bubble) = bubble_to_reorder {
                    v.push(bubble);
                }
            });
        }

        // ── session load start: clear UI, enter replay mode ────────────────
        Some("session_load_start") => {
            // Clear both destinations.  `messages` is what the user is
            // currently looking at; `replay_buffer` is where the incoming
            // flood will accumulate.  Keeping both empty here means the
            // single `messages.set(buffer)` on `session_load_end` is the
            // only `<For>` diff during the whole replay.
            messages.set(Vec::new());
            replay_buffer.set(Vec::new());
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
            // Single atomic swap: drains the buffer into messages in one
            // update so the keyed <For> performs exactly one diff for the
            // whole transcript.  std::mem::take avoids the temporary clone
            // we'd pay for with get_untracked() + set().
            let drained: Vec<ChatMessage> =
                replay_buffer.try_update(std::mem::take).unwrap_or_default();
            messages.set(drained);
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
            sessions.set(parse_session_entries(event));
        }

        // ── unbounded session list (View all sessions page) ────────────────
        //
        // Emitted by the host in response to the `list_all_sessions`
        // bridge command.  Drives `AllSessionsState::Loaded` so the
        // page re-renders with the full list.  We do *not* update the
        // `sessions` signal here — that's reserved for the truncated
        // sidebar list, which has its own update cadence.
        Some("all_sessions_list") => {
            all_sessions.set(AllSessionsState::Loaded(parse_session_entries(event)));
        }

        // Hosts may emit this to surface a human-readable failure when
        // the list fetch fails (e.g. transport closed mid-walk).  The
        // current host doesn't yet, but defining the variant up-front
        // means the UI works as soon as wiring lands without another
        // chat-UI release.
        Some("all_sessions_error") => {
            let message = js_sys::Reflect::get(event, &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "Failed to load sessions".to_string());
            all_sessions.set(AllSessionsState::Failed(message));
        }

        Some("session_active") => {
            let id = js_sys::Reflect::get(event, &JsValue::from_str("id"))
                .ok()
                .and_then(|v| v.as_string());
            active_session_id.set(id);
            // Reset chat state.  This event is fired:
            //   - on initial connect (no messages yet, no-op)
            //   - after `session_new` (new empty session)
            //   - immediately before `session_load_start` on a session switch
            //     (messages will be re-cleared and `replaying` set by the
            //     `session_load_start` arm; this just makes the sidebar's
            //     active highlight flip *now* rather than waiting for replay
            //     to finish)
            messages.set(Vec::new());
            replay_buffer.set(Vec::new());
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
