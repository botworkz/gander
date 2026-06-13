// SPDX-License-Identifier: GPL-3.0-or-later

//! Leptos chat client for gander.
//!
//! # Architecture
//!
//! ```
//! App
//! ├── Sidebar       (session list, "+ New session" button)
//! └── ChatPane
//!     ├── MessageList   (scrollable, one MessageView per message)
//!     └── input row     (textarea + Send button)
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
//! ## Session sidebar
//!
//! On startup the WASM sends `{type:"ready"}` to the host, which responds
//! with `{type:"session_list", sessions:[…]}` and
//! `{type:"session_active", id:"…", history:[]}`.  Clicking a session fires
//! `session_select`; clicking "+ New session" fires `session_new`.
//!
//! Note: `session/resume` in ACP v1 does not replay history, so
//! `session_active.history` is always `[]` in this version.
//!
//! # Entry point
//!
//! [`main`] is called by the Trunk-generated JS loader when the WASM
//! module is instantiated.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

pub mod bridge;
pub mod markdown;

/// Fallback label used when a session has no title.
///
/// Must match `ListedSession`'s fallback in `src/acp.rs` (different crate —
/// keep both in sync if this string ever changes).
const DEFAULT_SESSION_LABEL: &str = "Session";

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

/// A session entry shown in the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub label: String,
    /// ISO 8601 `updated_at` from the host, if available.
    pub last_active: Option<String>,
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
    messages: RwSignal<Vec<ChatMessage>>,
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
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

        Some("session_list") => {
            let raw_sessions = js_sys::Reflect::get(&event, &JsValue::from_str("sessions"))
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
            let id = js_sys::Reflect::get(&event, &JsValue::from_str("id"))
                .ok()
                .and_then(|v| v.as_string());
            active_session_id.set(id);
            // Clear chat messages when switching sessions.
            // History replay is not supported in ACP v1 session/resume.
            messages.set(Vec::new());
            in_flight.set(None);
            sending.set(false);
        }

        _ => {
            // Ignore unrecognised events so future protocol extensions are
            // forwards-compatible.
        }
    }
}

// ─── Time-ago formatting ──────────────────────────────────────────────────────

/// Format an ISO 8601 timestamp as a short "time ago" string.
///
/// Uses `js_sys::Date` to parse the timestamp and compute the elapsed days
/// accurately, including correct handling of variable-length months.
fn time_ago(iso: &str) -> String {
    let then = js_sys::Date::new(&JsValue::from_str(iso));
    // `Date::new` with an unparseable string produces NaN for `getTime()`.
    let then_ms = then.get_time();
    if then_ms.is_nan() {
        return iso.to_string();
    }

    let now_ms = js_sys::Date::now();
    let diff_ms = now_ms - then_ms;
    if diff_ms < 0.0 {
        return "just now".to_string();
    }

    let diff_mins = (diff_ms / 60_000.0) as u64;
    let diff_hours = diff_mins / 60;
    let diff_days = diff_hours / 24;

    match diff_mins {
        0..=1 => "just now".to_string(),
        2..=59 => format!("{diff_mins}m ago"),
        60..=119 => "1h ago".to_string(),
        _ if diff_hours < 24 => format!("{diff_hours}h ago"),
        _ if diff_days == 1 => "yesterday".to_string(),
        _ if diff_days < 7 => format!("{diff_days}d ago"),
        _ if diff_days < 14 => "1w ago".to_string(),
        _ if diff_days < 30 => format!("{}w ago", diff_days / 7),
        _ if diff_days < 365 => format!("{}mo ago", diff_days / 30),
        _ => format!("{}y ago", diff_days / 365),
    }
}

// ─── Components ───────────────────────────────────────────────────────────────

/// Root application component.
///
/// Owns all chat and session state and wires up the JS bridge subscription.
#[component]
pub fn App() -> impl IntoView {
    let messages: RwSignal<Vec<ChatMessage>> = RwSignal::new(Vec::new());
    let next_id: RwSignal<u32> = RwSignal::new(0);
    let input_text: RwSignal<String> = RwSignal::new(String::new());
    // True while an assistant reply is being streamed.
    let sending: RwSignal<bool> = RwSignal::new(false);
    // The assistant message currently receiving tokens, if any.
    let in_flight: RwSignal<Option<ChatMessage>> = RwSignal::new(None);
    // Session sidebar state.
    let sessions: RwSignal<Vec<SessionEntry>> = RwSignal::new(Vec::new());
    let active_session_id: RwSignal<Option<String>> = RwSignal::new(None);

    // Register the event callback once for the lifetime of the app.
    // The Closure is leaked intentionally — it must outlive the app.
    {
        let cb = Closure::wrap(Box::new(move |event: JsValue| {
            handle_bridge_event(
                event,
                in_flight,
                sending,
                messages,
                sessions,
                active_session_id,
            );
        }) as Box<dyn FnMut(JsValue)>);

        bridge::subscribe(cb.as_ref().unchecked_ref());
        cb.forget();
    }

    // Tell the host that the bridge is ready; it will respond with
    // session_list and session_active so the sidebar populates.
    bridge::post_json(r#"{"type":"ready"}"#);

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
        <div class="gander-root">
            <Sidebar sessions active_session_id />
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
        </div>
    }
}

/// Left-edge session sidebar.
///
/// Shows up to 5 sessions, a "+ New session" button, and highlights the
/// active session.
#[component]
fn Sidebar(
    sessions: RwSignal<Vec<SessionEntry>>,
    active_session_id: RwSignal<Option<String>>,
) -> impl IntoView {
    let on_new = move |_| {
        bridge::post_json(r#"{"type":"session_new"}"#);
    };

    view! {
        <div class="sidebar">
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
                            bridge::post_value(obj.into());
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
                                on:click=on_click
                            >
                                <span class="session-label">{label}</span>
                                <span class="session-ago">{ago}</span>
                            </button>
                        }
                    }
                />
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
