// SPDX-License-Identifier: GPL-3.0-or-later

//! Extension handler trait and associated types.
//!
//! [`ExtHandler`] is the boundary between the pure-ACP worker and
//! goose-specific extension logic.  The worker calls
//! [`ExtHandler::on_tool_call_completed`] for every completed tool call and
//! the handler decides whether to emit zero or more [`ExtEvent`]s.
//!
//! Today the only implementation is [`goose::GooseExtHandler`]; the trait
//! exists to give the worker an abstraction point without making it aware of
//! goose-private protocol details.

pub mod goose;

use agent_client_protocol::schema::{ToolCall, ToolCallId};
use tokio::sync::mpsc;

/// Inspect a completed [`ToolCall`] snapshot; may emit zero or more extension
/// events.
///
/// Fire-and-forget — must never gate the pure-ACP path.  If the handler
/// errors, the core worker logs at `warn!` and continues.  ACP correctness
/// must not depend on extension handlers.
#[async_trait::async_trait]
pub trait ExtHandler: Send + Sync {
    async fn on_tool_call_completed(&self, tc: &ToolCall, evt_tx: &mpsc::Sender<ExtEvent>);
}

/// Outbound work the extension handler wants the worker to perform on its
/// behalf.
#[derive(Debug)]
pub enum ExtRequest {
    /// Fetch a MCP App resource by URI and deliver the HTML to the UI.
    ReadResource {
        tool_call_id: ToolCallId,
        uri: String,
        extension_name: String,
    },
}

/// Events emitted by extension handlers.
#[derive(Debug)]
pub enum ExtEvent {
    /// The handler wants the worker to perform an outbound request.
    Request(ExtRequest),
    /// A goose-private session metadata frame for the footer bar.
    SessionInfo {
        cwd: String,
        model: String,
        tool_count: Option<u32>,
    },
    /// HTML panel fetched for a goose MCP App tool call.
    ToolResource { tool_call_id: String, html: String },
}
