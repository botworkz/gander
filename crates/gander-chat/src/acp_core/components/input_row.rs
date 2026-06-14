// SPDX-License-Identifier: GPL-3.0-or-later

//! Textarea + Send button input row.

use leptos::prelude::*;

use crate::acp_core::types::ChatMessage;

/// Textarea and Send button row at the bottom of the chat pane.
///
/// Owns the `submit` closure so that `App()` in `lib.rs` only needs to
/// thread the relevant signals through rather than assembling the DOM inline.
#[component]
pub fn InputRow(
    input_text: RwSignal<String>,
    sending: RwSignal<bool>,
    replaying: RwSignal<bool>,
    next_id: RwSignal<u32>,
    messages: RwSignal<Vec<ChatMessage>>,
    in_flight: RwSignal<Option<ChatMessage>>,
) -> impl IntoView {
    let submit = move || {
        let text = input_text.get_untracked().trim().to_string();
        if text.is_empty() || sending.get_untracked() || replaying.get_untracked() {
            return;
        }

        let uid = next_id.get_untracked();
        next_id.set(uid + 2); // reserve uid for user msg, uid+1 for assistant reply

        let user_msg = ChatMessage::new_user(uid, &text);
        let assistant_msg = ChatMessage::new_assistant(uid + 1);

        messages.update(|v| v.push(user_msg));
        messages.update(|v| v.push(assistant_msg));
        in_flight.set(Some(assistant_msg));
        sending.set(true);
        input_text.set(String::new());

        crate::bridge::send(&text);
    };

    // Whether input should be disabled.
    let input_disabled = move || sending.get() || replaying.get();

    view! {
        <div class="input-row">
            <textarea
                class="input-box"
                rows="3"
                placeholder=move || {
                    if replaying.get() { "Loading session…" } else { "Message…" }
                }
                prop:value=input_text
                prop:disabled=input_disabled
                on:input=move |ev| {
                    let el: web_sys::HtmlTextAreaElement =
                        ev.target().unwrap().unchecked_into();
                    input_text.set(el.value());
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    // Enter (without Shift) submits; Shift+Enter inserts a newline.
                    if ev.key() == "Enter" && !ev.shift_key() {
                        ev.prevent_default();
                        submit();
                    }
                }
            />
            <button
                class="send-btn"
                disabled=input_disabled
                on:click=move |_| submit()
            >
                "Send"
            </button>
        </div>
    }
}
