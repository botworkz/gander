// SPDX-License-Identifier: GPL-3.0-or-later

//! Goose-specific extension handler.
//!
//! [`GooseExtHandler`] implements [`crate::ext::ExtHandler`] and is the only
//! place in gander where `_meta.goose.*` and `_goose/unstable/resources/read`
//! are referenced.
//!
//! When a tool call with `rawOutput.resourceUri` and
//! `_meta.goose.toolCall.extensionName` completes, the handler emits an
//! [`ExtRequest::ReadResource`] event.  The worker drains the event channel
//! after each `drain_session_updates` call: `ReadResource` entries are
//! pushed onto `pending_fetches`; the worker then calls
//! [`process_pending_fetches`] which fires the actual RPC and emits
//! [`ExtEvent::ToolResource`] to the UI channel.
//!
//! ## Migration note
//!
//! When `_goose/unstable/resources/read` graduates to the ACP spec:
//! 1. Delete this file (or remove the resource-fetch logic from it).
//! 2. Add a `read_resource` method to `src/acp/`.
//! 3. Change `GooseExtHandler::on_tool_call_completed` to call it.
//! That's the entire migration; nothing else needs to change.

use std::sync::Arc;

use agent_client_protocol::{
    UntypedMessage,
    schema::{ToolCall, ToolCallId, ToolCallStatus},
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tracing::debug;

use crate::ext::{ExtEvent, ExtHandler, ExtRequest};

/// Pending MCP App resource fetches: (tool_call_id, resource_uri, extension_name).
///
/// Populated by the worker when it drains an [`ExtRequest::ReadResource`] from
/// the handler event channel; drained by [`process_pending_fetches`].
pub type PendingFetches = Arc<Mutex<Vec<(ToolCallId, String, String)>>>;

// ---------------------------------------------------------------------------
// GooseExtHandler
// ---------------------------------------------------------------------------

/// Extension handler for goose-specific ACP extensions.
///
/// Inspects completed tool calls for `rawOutput.resourceUri` and
/// `_meta.goose.toolCall.extensionName`.  When both are present it emits an
/// [`ExtRequest::ReadResource`] so the worker can fetch the HTML panel.
pub struct GooseExtHandler;

#[async_trait::async_trait]
impl ExtHandler for GooseExtHandler {
    async fn on_tool_call_completed(&self, tc: &ToolCall, evt_tx: &mpsc::Sender<ExtEvent>) {
        if tc.status != ToolCallStatus::Completed {
            return;
        }

        let resource_uri = tc
            .raw_output
            .as_ref()
            .and_then(|o| o.get("resourceUri"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let extension_name = tc
            .meta
            .as_ref()
            .and_then(|m| m.get("goose"))
            .and_then(|g| g.get("toolCall"))
            .and_then(|t| t.get("extensionName"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if let (Some(uri), Some(ext)) = (resource_uri, extension_name) {
            debug!(
                target: "gander::wire",
                tool_call_id = %tc.tool_call_id,
                resource_uri = %uri,
                extension_name = %ext,
                "QUEUE_READ_RESOURCE"
            );
            let _ = evt_tx
                .send(ExtEvent::Request(ExtRequest::ReadResource {
                    tool_call_id: tc.tool_call_id.clone(),
                    uri,
                    extension_name: ext,
                }))
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Resource fetch helpers
// ---------------------------------------------------------------------------

/// Extract the first `text/html` item from a `_goose/unstable/resources/read`
/// response.
///
/// Response shape (goose ≥ 1.37.0 `acp/server/resources.rs`):
///   `{ result: { contents: [{ uri, mimeType, text }] } }`
///
/// Returns `None` when:
/// - the `result` or `contents` keys are absent
/// - no item carries a `text/html` (or `text/html;…`) mime type
pub fn extract_html_from_read_resource_response(response: &Value) -> Option<String> {
    let items = response
        .get("result")
        .and_then(|r| r.get("contents"))
        .and_then(|c| c.as_array())?;
    items.iter().find_map(|item| {
        let mime = item.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
        if mime.starts_with("text/html") {
            item.get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// Drain `pending_fetches` and call `_goose/unstable/resources/read` for each.
///
/// On a `text/html` response, emits [`ExtEvent::ToolResource`] via `ext_ui_tx`.
/// Errors are logged and skipped — the tool-call card remains visible without
/// an iframe.
pub async fn process_pending_fetches<R>(
    cx: &agent_client_protocol::ConnectionTo<R>,
    ext_ui_tx: &mpsc::Sender<ExtEvent>,
    pending_fetches: &PendingFetches,
    session_id: String,
) where
    R: agent_client_protocol::role::Role,
    R: agent_client_protocol::role::HasPeer<R>,
{
    let fetches: Vec<(ToolCallId, String, String)> = {
        let mut guard = pending_fetches.lock().await;
        std::mem::take(&mut *guard)
    };

    for (tool_call_id, uri, extension_name) in fetches {
        let params = serde_json::json!({
            "sessionId": session_id,
            "uri": uri,
            "extensionName": extension_name,
        });
        let msg = match UntypedMessage::new("_goose/unstable/resources/read", params.clone()) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(%err, "failed to build _goose/unstable/resources/read request");
                continue;
            }
        };
        debug!(
            target: "gander::wire",
            direction = "send",
            method = "_goose/unstable/resources/read",
            payload = %params,
            "READ_RESOURCE_REQUEST"
        );
        let response: Value = match cx.send_request(msg).block_task().await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(%err, uri = %uri, "_goose/unstable/resources/read failed");
                continue;
            }
        };
        debug!(
            target: "gander::wire",
            direction = "recv",
            method = "_goose/unstable/resources/read",
            payload = %serde_json::to_string(&response).unwrap_or_default(),
            "READ_RESOURCE_RESPONSE"
        );
        if let Some(html) = extract_html_from_read_resource_response(&response) {
            let _ = ext_ui_tx
                .send(ExtEvent::ToolResource {
                    tool_call_id: tool_call_id.to_string(),
                    html,
                })
                .await;
        } else {
            tracing::warn!(uri = %uri, "_goose/unstable/resources/read returned no text/html content");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::apply_tool_call_update;
    use crate::ext::{ExtEvent, ExtRequest};
    use agent_client_protocol::schema::{
        ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdateFields,
    };

    // Helper: call on_tool_call_completed and collect emitted ExtEvents.
    async fn emitted_events(tc: &ToolCall) -> Vec<ExtEvent> {
        let (tx, mut rx) = mpsc::channel(8);
        GooseExtHandler.on_tool_call_completed(tc, &tx).await;
        drop(tx);
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(evt);
        }
        events
    }

    #[tokio::test]
    async fn completed_tool_without_resource_uri_does_not_emit() {
        let tc = ToolCall::new(ToolCallId::new("tc-plain"), "plain_tool");
        let mut tc = tc;
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        apply_tool_call_update(&mut tc, fields);

        let events = emitted_events(&tc).await;
        assert!(
            events.is_empty(),
            "no resourceUri means no emit; got {events:?}"
        );
    }

    #[tokio::test]
    async fn completed_tool_with_resource_uri_but_no_extension_does_not_emit() {
        let tc = ToolCall::new(ToolCallId::new("tc-no-ext"), "mcp_tool");
        let mut tc = tc;
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://ext/resource"}));
        apply_tool_call_update(&mut tc, fields);

        let events = emitted_events(&tc).await;
        assert!(
            events.is_empty(),
            "missing extensionName means no emit; got {events:?}"
        );
    }

    #[tokio::test]
    async fn completed_tool_with_resource_uri_and_extension_emits_request() {
        let meta_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "goose": { "toolCall": { "extensionName": "my_ext" } }
            }))
            .unwrap();
        let mut tc = ToolCall::new(ToolCallId::new("tc-emit"), "mcp_tool").meta(meta_map);
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://my_ext/panel"}));
        apply_tool_call_update(&mut tc, fields);

        let events = emitted_events(&tc).await;
        assert_eq!(events.len(), 1, "expected exactly one emitted event");
        match &events[0] {
            ExtEvent::Request(ExtRequest::ReadResource {
                tool_call_id,
                uri,
                extension_name,
            }) => {
                assert_eq!(tool_call_id.to_string(), "tc-emit");
                assert_eq!(uri, "mcp://my_ext/panel");
                assert_eq!(extension_name, "my_ext");
            }
            other => panic!("expected ReadResource request, got {other:?}"),
        }
    }

    // goose-ext: extensionName is delivered on ToolCall creation and must
    // survive a later update that carries rawOutput but no meta.
    #[tokio::test]
    async fn meta_merge_preserves_extension_name_across_updates() {
        let meta_map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "goose": { "toolCall": { "extensionName": "keep_me" } }
            }))
            .unwrap();
        let mut tc = ToolCall::new(ToolCallId::new("tc-merge"), "mcp_tool").meta(meta_map);

        // First update: no meta, no status change — extensionName must persist.
        let fields1 = ToolCallUpdateFields::new().raw_output(serde_json::json!({"partial": true}));
        apply_tool_call_update(&mut tc, fields1);

        // Simulate the top-level meta merge that apply_tool_call_update does NOT do
        // (meta merge happens in forward_update; here we build the snapshot directly).
        // The meta on the ToolCall already carries "keep_me" from creation.

        // Second update: Completed + resourceUri — extensionName must still be present.
        let fields2 = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "mcp://keep_me/x"}));
        apply_tool_call_update(&mut tc, fields2);

        let events = emitted_events(&tc).await;
        assert_eq!(events.len(), 1, "extensionName must survive apply sequence");
        match &events[0] {
            ExtEvent::Request(ExtRequest::ReadResource { extension_name, .. }) => {
                assert_eq!(extension_name, "keep_me");
            }
            other => panic!("expected ReadResource, got {other:?}"),
        }
    }

    // ── extract_html_from_read_resource_response ──────────────────────────

    #[test]
    fn extract_html_returns_text_for_text_html_mime() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "text/html", "text": "<p>hi</p>", "uri": "x" }]
            }
        });
        assert_eq!(
            extract_html_from_read_resource_response(&resp),
            Some("<p>hi</p>".to_string())
        );
    }

    #[test]
    fn extract_html_accepts_text_html_profile_variant() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "text/html;profile=mcp-app", "text": "<div/>", "uri": "x" }]
            }
        });
        assert_eq!(
            extract_html_from_read_resource_response(&resp),
            Some("<div/>".to_string())
        );
    }

    #[test]
    fn extract_html_returns_none_for_non_html_mime() {
        let resp = serde_json::json!({
            "result": {
                "contents": [{ "mimeType": "application/json", "text": "{}", "uri": "x" }]
            }
        });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }

    #[test]
    fn extract_html_returns_none_when_contents_empty() {
        let resp = serde_json::json!({ "result": { "contents": [] } });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }

    #[test]
    fn extract_html_returns_none_when_result_key_missing() {
        let resp = serde_json::json!({ "other": {} });
        assert!(extract_html_from_read_resource_response(&resp).is_none());
    }
}
