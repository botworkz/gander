// SPDX-License-Identifier: GPL-3.0-or-later

//! Full sessions listing — replaces the chat pane when active.
//!
//! The sidebar's Sessions section is intentionally short (top N items
//! recently active, currently 5) so that Extensions / Settings still fit on
//! a typical viewport.  When the user wants to find a session that fell off
//! the bottom they click "View all sessions" in the sidebar, which sets
//! `ChatPaneView::AllSessions` and brings us here instead of the chat
//! conversation.
//!
//! ## What this currently shows
//!
//! A header (title + back button) plus the same `sessions` list the sidebar
//! uses, in a vertical row layout with id + label + `time_ago(last_active)`.
//! This is the *list* view shape — deliberately not the card-per-session
//! grid that some agents render; at this stage scanning chronologically is
//! more important than visual chrome.
//!
//! ## What this will eventually show
//!
//! A search-and-filterable list of every session the agent knows about for
//! this profile, fetched on demand.  The current placeholder reuses the
//! truncated-to-5 sidebar list because the unbounded fetch path doesn't
//! exist yet on the host side; wiring that up is a follow-up.  The view
//! shape itself (header, list, back button, click-to-load) is what we
//! want to land first so the navigation flow is real.
//!
//! ## Lives in `acp_core`
//!
//! Sessions are pure-ACP state (`session/list`, `session/load`,
//! `session/new`), so the rendering stays in `acp_core/components`.  No
//! extension-private references — the CI grep guard verifies.

use leptos::ev;
use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::JsValue;

use crate::acp_core::components::time_ago::time_ago;
use crate::acp_core::types::{ChatPaneView, SessionEntry};

/// Full sessions listing page.
///
/// `view` is the shared pane-view signal owned by `App`; we flip it back
/// to [`ChatPaneView::Chat`] when the user clicks "Back" or selects a
/// session (the existing `session_active` event handler then takes over
/// loading the conversation into the now-visible chat pane).
#[component]
pub fn AllSessions(
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
    view: RwSignal<ChatPaneView>,
) -> impl IntoView {
    let on_back = move |_| view.set(ChatPaneView::Chat);

    // Enter / Space activate the back button when focused; the browser
    // already does this for <button>, but rendering an explicit handler
    // keeps the contract obvious if the element shape changes later.
    let on_back_keydown = move |ev: ev::KeyboardEvent| {
        if ev.key() == "Enter" || ev.key() == " " {
            ev.prevent_default();
            view.set(ChatPaneView::Chat);
        }
    };

    view! {
        <div class="all-sessions">
            <div class="all-sessions-header">
                <button
                    class="all-sessions-back"
                    title="Back to chat"
                    aria-label="Back to chat"
                    on:click=on_back
                    on:keydown=on_back_keydown
                >
                    <Icon icon=icondata::LuArrowLeft width="16px" height="16px" />
                </button>
                <h2 class="all-sessions-title">"All sessions"</h2>
            </div>

            // Placeholder banner — surfaced in-view rather than only in a
            // code comment so the limitation is obvious during dev and to
            // anyone reviewing this PR; will go away once the unbounded
            // list path is wired up.
            <div class="all-sessions-placeholder">
                "Showing only the most-recent sessions for now. "
                "Full history list is wired up in a follow-up."
            </div>

            <div class="all-sessions-list">
                <For
                    each=move || sessions.get()
                    key=|s| s.id.clone()
                    children=move |entry: SessionEntry| {
                        // Two String clones because `is_active` captures
                        // `id` by value and `on_click` builds the
                        // `session_select` payload from `id_for_click` —
                        // mirrors the per-row pattern in `sidebar.rs`.
                        let id = entry.id.clone();
                        let label = entry.label.clone();
                        let ago = entry
                            .last_active
                            .as_deref()
                            .map(time_ago)
                            .unwrap_or_default();
                        let id_for_click = id.clone();
                        let is_active = move || {
                            active_session_id
                                .get()
                                .as_deref()
                                .map(|a| a == id.as_str())
                                .unwrap_or(false)
                        };
                        let on_click = move |_| {
                            // Build the message as a proper JS object to
                            // avoid manual JSON escaping and potential
                            // injection — same pattern as sidebar.rs.
                            let obj = js_sys::Object::new();
                            let _ = js_sys::Reflect::set(
                                &obj,
                                &JsValue::from_str("type"),
                                &JsValue::from_str("session_select"),
                            );
                            let _ = js_sys::Reflect::set(
                                &obj,
                                &JsValue::from_str("id"),
                                &JsValue::from_str(&id_for_click),
                            );
                            crate::bridge::post_value(obj.into());
                            // Flip back to the chat pane immediately so
                            // the user lands on the conversation when
                            // history replay finishes.  The chat pane is
                            // already cleared by the `session_active`
                            // event handler that fires before
                            // `session_load_start`.
                            view.set(ChatPaneView::Chat);
                        };
                        view! {
                            <button
                                class=move || {
                                    if is_active() {
                                        "all-sessions-row all-sessions-row--active"
                                    } else {
                                        "all-sessions-row"
                                    }
                                }
                                on:click=on_click
                            >
                                <span class="all-sessions-row-icon">
                                    <Icon
                                        icon=icondata::LuMessageCircle
                                        width="16px"
                                        height="16px"
                                    />
                                </span>
                                <span class="all-sessions-row-label">{label}</span>
                                <span class="all-sessions-row-ago">{ago}</span>
                            </button>
                        }
                    }
                />
            </div>
        </div>
    }
}
