// SPDX-License-Identifier: GPL-3.0-or-later

//! Left-edge session list (the upper portion of the sidebar chrome).
//!
//! Renders a single concertina-style section whose header reads
//! `[💬] Sessions [+] [›]`:
//!
//! - clicking the **row body** toggles the section open/closed, revealing
//!   or hiding the session list
//! - clicking the **`+` action** posts `session_new` and force-opens the
//!   section so the new session lands visibly at the top of the list
//!
//! The CSS reuses `.concertina-*` from the extension-side accordion
//! below so the three sections (Sessions, Extensions, Settings) feel
//! like one menu even though the Sessions section is owned by
//! `acp_core` and the other two by the extension layer.
//!
//! Open state lives in a local `RwSignal` because the section is
//! independent of the extension-side concertina's single-open-at-a-time
//! rule — sessions are the primary navigation surface and the user is
//! likely to want them visible alongside Extensions or Settings.

use leptos::ev;
use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::JsValue;

use crate::acp_core::components::time_ago::time_ago;
use crate::acp_core::types::SessionEntry;

/// Left-edge session sidebar content.
///
/// Renders the Sessions concertina row plus the session list body when open.
/// Composed alongside the extension-side `Concertina` (Extensions / Settings)
/// by `App()` in `lib.rs`.
#[component]
pub fn Sidebar(
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
) -> impl IntoView {
    // Sessions default to open: this is the primary navigation surface,
    // unlike Extensions/Settings which start collapsed because they hold
    // rarely-used config.
    let open: RwSignal<bool> = RwSignal::new(true);

    let on_toggle = move |_| open.update(|o| *o = !*o);

    // Keyboard a11y on the row.  The row itself is a <div role="button">
    // because it needs to contain another <button> (the `+` action), which
    // is not legal inside a real <button>.
    let on_keydown = move |ev: ev::KeyboardEvent| {
        if ev.key() == "Enter" || ev.key() == " " {
            ev.prevent_default();
            open.update(|o| *o = !*o);
        }
    };

    let on_new = move |ev: ev::MouseEvent| {
        // Don't let the click bubble up to the row's toggle handler — the
        // `+` is an action, not a way to open/close the list.
        ev.stop_propagation();
        crate::bridge::post_json(r#"{"type":"session_new"}"#);
        // Force-open so the user immediately sees the new session land at
        // the top of the list, even if they'd previously collapsed it.
        open.set(true);
    };

    view! {
        <div class="concertina-section">
            <div
                class=move || {
                    if open.get() {
                        "concertina-row concertina-row--open"
                    } else {
                        "concertina-row"
                    }
                }
                role="button"
                tabindex="0"
                on:click=on_toggle
                on:keydown=on_keydown
            >
                <span class="concertina-icon">
                    <Icon
                        icon=icondata::LuMessageCircle
                        width="15px"
                        height="15px"
                    />
                </span>
                <span class="concertina-label">"Sessions"</span>
                <button
                    class="concertina-action"
                    title="New session"
                    aria-label="New session"
                    on:click=on_new
                >
                    <Icon icon=icondata::LuPlus width="14px" height="14px" />
                </button>
                <span class=move || {
                    if open.get() {
                        "concertina-chevron concertina-chevron--open"
                    } else {
                        "concertina-chevron"
                    }
                }>
                    <Icon
                        icon=icondata::LuChevronRight
                        width="14px"
                        height="14px"
                    />
                </span>
            </div>
            {move || {
                open.get()
                    .then(|| {
                        view! {
                            // No padding here so the per-item buttons can
                            // border-bottom edge-to-edge like the old list.
                            <div class="concertina-content concertina-content--list">
                                <div class="session-list">
                                    <For
                                        each=move || sessions.get()
                                        key=|s| s.id.clone()
                                        children=move |entry| {
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
                                                // Build the message as a proper JS object to avoid
                                                // manual JSON escaping and potential injection.
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
                                            };
                                            view! {
                                                <button
                                                    class=move || {
                                                        if is_active() {
                                                            "session-item session-item--active"
                                                        } else {
                                                            "session-item"
                                                        }
                                                    }
                                                    title=ago.clone()
                                                    on:click=on_click
                                                >
                                                    <span class="session-icon">
                                                        <Icon
                                                            icon=icondata::LuMessageCircle
                                                            width="14px"
                                                            height="14px"
                                                        />
                                                    </span>
                                                    <span class="session-label">{label}</span>
                                                </button>
                                            }
                                        }
                                    />
                                </div>
                            </div>
                        }
                    })
            }}
        </div>
    }
}
