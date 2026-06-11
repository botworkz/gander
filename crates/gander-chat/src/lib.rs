// SPDX-License-Identifier: GPL-3.0-or-later

//! Leptos chat client for gander.
//!
//! # Architecture
//!
//! ```
//! App
//! ├── MessageList   (scrollable, one MessageView per message)
//! │   └── MessageView  (user bubble / assistant bubble)
//! └── input row     (textarea + Send button)
//! ```
//!
//! All chat state lives in [`App`] as [`leptos::RwSignal`]s.  Because
//! Leptos signals are `Copy`, they can be freely captured in closures
//! without reference-counting.
//!
//! ## Streaming
//!
//! Streaming token updates work like this:
//!
//! 1. User submits text → `bridge::send(text)` called.
//! 2. An in-flight [`ChatMessage`] (role = Assistant, `streaming = true`)
//!    is appended to the message list.
//! 3. The JS bridge fires `{type:"token", content:"…"}` events.
//! 4. Each token calls `msg.content.update(|c| c.push_str(token))`.
//! 5. Leptos patches only the single changed text node — no vdom diff.
//! 6. A `{type:"done"}` event marks the message complete.
//!
//! # Entry point
//!
//! [`main`] is called by the Trunk-generated JS loader when the WASM
//! module is instantiated.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

pub mod bridge;
pub mod markdown;

// ─── Data model ──────────────────────────────────────────────────────────────

/// Whether a message was written by the user or the assistant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// A single chat message.
///
/// All mutable fields are [`RwSignal`]s so that Leptos can surgically update
/// the DOM when tokens arrive, without diffing the whole message list.
///
/// The struct is [`Copy`] because every field is either a primitive or an
/// `RwSignal<T>` (which is itself `Copy` — it's just a typed arena ID).
#[derive(Clone, Copy, Debug)]
pub struct ChatMessage {
    /// Stable, unique ID used as the `key` in `<For>` to avoid re-mounts.
    pub id: u32,
    pub role: Role,
    /// Accumulated text.  Grows token-by-token for in-flight messages.
    pub content: RwSignal<String>,
    /// `true` while the host is still streaming tokens for this message.
    pub streaming: RwSignal<bool>,
    /// Non-`None` when the stream ended with an error.
    pub error: RwSignal<Option<String>>,
}

impl ChatMessage {
    fn new_user(id: u32, text: &str) -> Self {
        Self {
            id,
            role: Role::User,
            content: RwSignal::new(text.to_string()),
            streaming: RwSignal::new(false),
            error: RwSignal::new(None),
        }
    }

    fn new_assistant(id: u32) -> Self {
        Self {
            id,
            role: Role::Assistant,
            content: RwSignal::new(String::new()),
            streaming: RwSignal::new(true),
            error: RwSignal::new(None),
        }
    }
}

// ─── Event handling ───────────────────────────────────────────────────────────

/// Dispatch a raw JS bridge event to the appropriate signal update.
///
/// Called from the `Closure` registered with `bridge::subscribe`.
/// Runs outside any reactive context, so signal reads use `get_untracked`.
fn handle_bridge_event(
    event: JsValue,
    in_flight: RwSignal<Option<ChatMessage>>,
    sending: RwSignal<bool>,
) {
    let event_type = js_sys::Reflect::get(&event, &JsValue::from_str("type"))
        .ok()
        .and_then(|v| v.as_string());

    match event_type.as_deref() {
        Some("token") => {
            let token = js_sys::Reflect::get(&event, &JsValue::from_str("content"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            if let Some(msg) = in_flight.get_untracked() {
                msg.content.update(|c| c.push_str(&token));
            }
        }

        Some("done") => {
            if let Some(msg) = in_flight.get_untracked() {
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
        }

        Some("error") => {
            let message = js_sys::Reflect::get(&event, &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "Unknown error".to_string());

            if let Some(msg) = in_flight.get_untracked() {
                msg.error.set(Some(message));
                msg.streaming.set(false);
            }
            in_flight.set(None);
            sending.set(false);
        }

        _ => {
            // Ignore unrecognised events so future protocol extensions are
            // forwards-compatible.
        }
    }
}

// ─── Components ───────────────────────────────────────────────────────────────

/// Root application component.
///
/// Owns all chat state and wires up the JS bridge subscription.
#[component]
pub fn App() -> impl IntoView {
    let messages: RwSignal<Vec<ChatMessage>> = RwSignal::new(Vec::new());
    let next_id: RwSignal<u32> = RwSignal::new(0);
    let input_text: RwSignal<String> = RwSignal::new(String::new());
    // True while an assistant reply is being streamed.
    let sending: RwSignal<bool> = RwSignal::new(false);
    // The assistant message currently receiving tokens, if any.
    let in_flight: RwSignal<Option<ChatMessage>> = RwSignal::new(None);

    // Register the event callback once for the lifetime of the app.
    // The Closure is leaked intentionally — it must outlive the app.
    {
        let cb = Closure::wrap(Box::new(move |event: JsValue| {
            handle_bridge_event(event, in_flight, sending);
        }) as Box<dyn FnMut(JsValue)>);

        bridge::subscribe(cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Submit the current input as a user message.
    let submit = move || {
        let text = input_text.get_untracked().trim().to_string();
        if text.is_empty() || sending.get_untracked() {
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

        bridge::send(&text);
    };

    view! {
        <div class="gander-chat">
            <MessageList messages />
            <div class="input-row">
                <textarea
                    class="input-box"
                    rows="3"
                    placeholder="Message…"
                    prop:value=input_text
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
                    disabled=move || sending.get()
                    on:click=move |_| submit()
                >
                    "Send"
                </button>
            </div>
        </div>
    }
}

/// Scrollable list of all chat messages.
#[component]
fn MessageList(messages: RwSignal<Vec<ChatMessage>>) -> impl IntoView {
    view! {
        <div class="message-list">
            <For
                each=move || messages.get()
                key=|msg| msg.id
                children=|msg| view! { <MessageView message=msg /> }
            />
        </div>
    }
}

/// A single message bubble (user or assistant).
///
/// The `inner_html` on `.message-content` lets pulldown-cmark HTML land
/// directly in the DOM.  Content is trusted (local bridge only).
#[component]
fn MessageView(message: ChatMessage) -> impl IntoView {
    let role_class = match message.role {
        Role::User => "message message--user",
        Role::Assistant => "message message--assistant",
    };

    view! {
        <div class=role_class>
            // Reactive inner HTML: re-evaluates only when `content` changes.
            <div
                class="message-content"
                inner_html=move || markdown::render(&message.content.get())
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
    }
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Application entry point called by the Trunk-generated JS loader.
///
/// Marked `#[wasm_bindgen(start)]` so that wasm-bindgen exports it as the
/// WASM module start function; the Trunk-generated JS calls it automatically
/// when the module is instantiated.
#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
