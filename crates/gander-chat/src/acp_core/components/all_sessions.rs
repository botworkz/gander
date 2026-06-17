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
//! ## Fetch lifecycle
//!
//! The unbounded session list comes from the host on demand, not as part
//! of the `ready` handshake.  The full lifecycle:
//!
//! 1. The user clicks "View all sessions" → `pane_view` flips to
//!    `AllSessions` → this component mounts.
//! 2. On mount, if `state == Idle`, we flip to `Loading` and post
//!    `{type:"list_all_sessions"}` to the host.
//! 3. The host walks `session/list` to completion and sends back
//!    `{type:"all_sessions_list", sessions:[…]}`, which the
//!    `acp_core::events` handler turns into `Loaded(_)`.
//! 4. The view re-renders with the full list.
//!
//! Why "on mount" rather than "on click in the sidebar": the sidebar
//! link doesn't know about `AllSessionsState`, and putting the fetch
//! trigger in the page itself means any future entry point (search
//! shortcut, deep link, …) gets the same behaviour for free.
//!
//! Why fetch is sticky (state survives back-to-Chat then forward
//! again): the user toggling between chat and the all-sessions page
//! shouldn't cause repeated round-trips to the agent.  A `Refresh`
//! button forces a re-fetch when staleness matters.
//!
//! ## View shape
//!
//! Vertical list, one row per session — deliberately not a card grid.
//! Sessions are looked up by recency or fragment-of-title; a tall
//! list scans far faster than a grid for that pattern.  We can revisit
//! once sessions grow side-channel metadata (pins, tags, etc.) that
//! justifies the extra real estate.
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
use crate::acp_core::types::{AllSessionsState, ChatPaneView, SessionEntry};

/// Post `{type:"list_all_sessions"}` to the host and flip `state` to
/// `Loading`.  The host replies with an `all_sessions_list` (or
/// `all_sessions_error`) bridge event, which the events module
/// converts into `Loaded(_)` / `Failed(_)`.
fn request_all_sessions(state: RwSignal<AllSessionsState>) {
    state.set(AllSessionsState::Loading);
    crate::bridge::post_json(r#"{"type":"list_all_sessions"}"#);
}

/// Full sessions listing page.
///
/// `view` is the shared pane-view signal owned by `App`; we flip it back
/// to [`ChatPaneView::Chat`] when the user clicks "Back" or selects a
/// session (the existing `session_active` event handler then takes over
/// loading the conversation into the now-visible chat pane).
///
/// `state` is the lifecycle of the unbounded fetch.  Owned by `App` so
/// it survives the user toggling between chat and all-sessions: a
/// successful `Loaded(_)` stays cached until the user explicitly
/// refreshes.
#[component]
pub fn AllSessions(
    active_session_id: RwSignal<Option<String>>,
    view: RwSignal<ChatPaneView>,
    state: RwSignal<AllSessionsState>,
) -> impl IntoView {
    // Auto-fetch on first mount.  Subsequent mounts (back-and-forth
    // between chat and this page) are no-ops — the user explicitly
    // refreshes via the button if they want fresh data.
    if matches!(state.get_untracked(), AllSessionsState::Idle) {
        request_all_sessions(state);
    }

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

    let on_refresh = move |_| request_all_sessions(state);

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
                // Right-justify the refresh button.  Visual sibling of
                // `.all-sessions-back` (same square chrome) but pinned
                // to the trailing edge of the header so it doesn't
                // crowd the title.
                <button
                    class="all-sessions-back all-sessions-refresh"
                    title="Refresh sessions list"
                    aria-label="Refresh sessions list"
                    on:click=on_refresh
                >
                    <Icon icon=icondata::LuRefreshCw width="14px" height="14px" />
                </button>
            </div>

            // The body switches on the fetch lifecycle.  Each branch
            // owns its own DOM rather than sharing a list container so
            // we can render placeholders that aren't visually
            // misleading (a spinner inside an empty list looks broken).
            {move || {
                match state.get() {
                    AllSessionsState::Idle => {
                        // Should be unreachable in practice — `mount`
                        // above flips Idle → Loading immediately.
                        // Render the same skeleton as Loading so a
                        // hypothetical race doesn't show a blank pane.
                        view! {
                            <div class="all-sessions-status">
                                "Preparing…"
                            </div>
                        }.into_any()
                    }
                    AllSessionsState::Loading => view! {
                        <div class="all-sessions-status all-sessions-status--loading">
                            "Loading sessions…"
                        </div>
                    }.into_any(),
                    AllSessionsState::Failed(message) => view! {
                        <div class="all-sessions-status all-sessions-status--error">
                            <span class="all-sessions-status-label">
                                "Couldn't load sessions: "
                            </span>
                            <span class="all-sessions-status-detail">{message}</span>
                            <button
                                class="all-sessions-retry"
                                on:click=on_refresh
                            >
                                "Retry"
                            </button>
                        </div>
                    }.into_any(),
                    AllSessionsState::Loaded(entries) => {
                        if entries.is_empty() {
                            view! {
                                <div class="all-sessions-status">
                                    "No sessions yet for this profile."
                                </div>
                            }.into_any()
                        } else {
                            // The actual list view — what the user
                            // really came here for.  Per-row click
                            // handler mirrors the sidebar's pattern.
                            view! {
                                <SessionListView
                                    entries=entries
                                    active_session_id=active_session_id
                                    pane_view=view
                                />
                            }.into_any()
                        }
                    }
                }
            }}
        </div>
    }
}

/// Inner list view that renders the loaded sessions.
///
/// Split out so the parent's `match` arms stay readable — the loading,
/// error, and empty branches each hand back a single small `<div>`
/// while the loaded branch hands back this richer subtree.
#[component]
fn SessionListView(
    /// Snapshot from `AllSessionsState::Loaded(_)` — owned, not a
    /// signal, because the lifecycle resets the entire Vec on every
    /// state transition (we never patch a Loaded list in place).
    entries: Vec<SessionEntry>,
    active_session_id: RwSignal<Option<String>>,
    pane_view: RwSignal<ChatPaneView>,
) -> impl IntoView {
    // Total count is informative when scanning a long list; surfacing
    // it up-front also makes the "1 session" / "203 sessions" jump
    // obvious if a fetch returns surprising data.
    let count = entries.len();
    let count_label = if count == 1 {
        "1 session".to_string()
    } else {
        format!("{count} sessions")
    };

    view! {
        <div class="all-sessions-count">{count_label}</div>
        <div class="all-sessions-list">
            <For
                each=move || entries.clone()
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
                        pane_view.set(ChatPaneView::Chat);
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
    }
}
