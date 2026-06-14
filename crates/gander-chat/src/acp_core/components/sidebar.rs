// SPDX-License-Identifier: GPL-3.0-or-later

//! Left-edge session list (the upper portion of the sidebar chrome).

use leptos::prelude::*;
use leptos_icons::Icon;
use wasm_bindgen::JsValue;

use crate::acp_core::components::time_ago::time_ago;
use crate::acp_core::types::SessionEntry;

/// Left-edge session sidebar content.
///
/// Renders the "+ New session" button and the session list.  The
/// collapsible concertina menu lives in the extension layer and is composed
/// into the sidebar wrapper by `App()` in `lib.rs`.
#[component]
pub fn Sidebar(
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
) -> impl IntoView {
    let on_new = move |_| {
        crate::bridge::post_json(r#"{"type":"session_new"}"#);
    };

    view! {
        <>
            <button class="new-session-btn" on:click=on_new>
                "+ New session"
            </button>
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
        </>
    }
}
