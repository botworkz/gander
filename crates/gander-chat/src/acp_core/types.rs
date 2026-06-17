// SPDX-License-Identifier: GPL-3.0-or-later

//! Core ACP data model: message and session types.

use leptos::prelude::*;

/// Fallback label used when a session has no title.
///
/// Must match `ListedSession`'s fallback in `src/acp/mod.rs` (different crate —
/// keep both in sync if this string ever changes).
pub const DEFAULT_SESSION_LABEL: &str = "Session";

// ─── Data model ──────────────────────────────────────────────────────────────

/// Whether a message was written by the user or the assistant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// A tool invocation or result (gray, monospace-feel).
    Tool,
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
    /// For `Role::Tool` messages this holds the full serialised `ToolCall`
    /// JSON snapshot, replaced on every update.
    pub content: RwSignal<String>,
    /// `true` while the host is still streaming tokens for this message.
    /// For tool messages, `true` means the tool call is still in progress.
    pub streaming: RwSignal<bool>,
    /// Non-`None` when the stream ended with an error.
    pub error: RwSignal<Option<String>>,
    /// For `Role::Tool` messages: the ACP `tool_call_id` string, used to
    /// match subsequent update snapshots to the right card.  `None` for
    /// user and assistant messages.
    pub tool_call_id: RwSignal<Option<String>>,
    /// Pre-hydrated HTML from an MCP App attachment.  `None` until the host
    /// fires a `tool_resource` bridge event.  When `Some`, the card renders
    /// a sandboxed iframe.
    // Field is populated exclusively by the extension layer; kept inline
    // until the ExtHandler trait provides a generic ext-slot (sibling PR #85).
    pub ui_html: RwSignal<Option<String>>,
    /// `true` while a UI resource fetch is in flight (i.e. `rawOutput.resourceUri`
    /// was detected on the completed tool call but `tool_resource` has not yet
    /// arrived).  Drives a loading placeholder in the iframe slot so the card
    /// layout does not shift when the HTML arrives.
    pub ui_pending: RwSignal<bool>,
}

impl ChatMessage {
    pub(crate) fn new_user(id: u32, text: &str) -> Self {
        Self {
            id,
            role: Role::User,
            content: RwSignal::new(text.to_string()),
            streaming: RwSignal::new(false),
            error: RwSignal::new(None),
            tool_call_id: RwSignal::new(None),
            ui_html: RwSignal::new(None),
            ui_pending: RwSignal::new(false),
        }
    }

    pub(crate) fn new_assistant(id: u32) -> Self {
        Self {
            id,
            role: Role::Assistant,
            content: RwSignal::new(String::new()),
            streaming: RwSignal::new(true),
            error: RwSignal::new(None),
            tool_call_id: RwSignal::new(None),
            ui_html: RwSignal::new(None),
            ui_pending: RwSignal::new(false),
        }
    }

    pub(crate) fn new_tool(id: u32, tool_call_id: String, json: String) -> Self {
        Self {
            id,
            role: Role::Tool,
            content: RwSignal::new(json),
            // Tool call is in progress until we receive a terminal status.
            streaming: RwSignal::new(true),
            error: RwSignal::new(None),
            tool_call_id: RwSignal::new(Some(tool_call_id)),
            ui_html: RwSignal::new(None),
            ui_pending: RwSignal::new(false),
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

/// Which view the right-hand pane is currently rendering.
///
/// The sidebar is always visible; this only affects what fills the
/// pane to its right.  Driven by an [`RwSignal`] in `App` so any
/// component can flip it (sidebar's "View all" link, the back button
/// on the all-sessions page, the `session_active` event handler).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatPaneView {
    /// The chat conversation (MessageList + InputRow + Footer) — default.
    Chat,
    /// The full sessions listing (currently a placeholder; eventually
    /// the host will fetch the unbounded list and we'll render it here).
    AllSessions,
}
