// SPDX-License-Identifier: GPL-3.0-or-later

//! Goose-specific extension handler.
//!
//! [`GooseExtHandler`] implements [`crate::ext::ExtHandler`] and is the only
//! place in gander where `_meta.goose.*` and `_goose/unstable/resources/read`
//! are referenced.
//!
//! When a tool call with `rawOutput.resourceUri` completes, the handler
//! emits an [`ExtRequest::ReadResource`] event.  The worker drains the
//! event channel after each `drain_session_updates` call: `ReadResource`
//! entries are pushed onto `pending_fetches`; the worker then calls
//! [`process_pending_fetches`] which fires the actual RPC and emits
//! [`ExtEvent::ToolResource`] to the UI channel.
//!
//! The extension name attached to the fetch comes from
//! `_meta.goose.toolCall.extensionName` when present (live tool calls).  On
//! history replay goose currently omits the meta block, so we fall back to
//! the host component of the resource URI (see [`extension_name_from_uri`]).
//!
//! ## Per-process resource cache
//!
//! [`process_pending_fetches`] consults a [`ResourceCache`] (keyed by
//! `(session_id, uri)`) before issuing each RPC.  Cache hits are emitted
//! directly as [`ExtEvent::ToolResource`] without a network round-trip; cache
//! misses populate the cache after a successful fetch.
//!
//! The cache is owned by the per-tab ACP worker and lives for the tab's
//! lifetime.  It exists to satisfy gander#101: re-opening the same session
//! within one gander run must not issue redundant
//! `_goose/unstable/resources/read` calls.  Cache invalidation across panel
//! changes on the goose side is intentionally out of scope (panels are
//! deterministic given `(session_id, uri)` in current goose versions).
//!
//! ## Migration note
//!
//! When `_goose/unstable/resources/read` graduates to the ACP spec:
//! 1. Delete this file (or remove the resource-fetch logic from it).
//! 2. Add a `read_resource` method to `src/acp/`.
//! 3. Change `GooseExtHandler::on_tool_call_completed` to call it.
//!
//! That's the entire migration; nothing else needs to change.

use std::collections::HashMap;
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

/// Per-process resource cache: `(session_id, uri) → html`.
///
/// Owned by the ACP worker, lives for the tab's lifetime.  Populated by
/// [`process_pending_fetches`] after a successful fetch; consulted at the top
/// of the same function on subsequent fetches.
///
/// See the module docs for the design rationale (gander#101).
pub type ResourceCache = Arc<Mutex<HashMap<(String, String), String>>>;

/// Build an empty [`ResourceCache`].
///
/// Provided so the worker doesn't have to spell out the inner type.
pub fn new_resource_cache() -> ResourceCache {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// GooseExtHandler
// ---------------------------------------------------------------------------

/// Extract the extension name from the host component of a panel URI.
///
/// Goose's panel URIs follow `scheme://<extension>/<path>` (e.g.
/// `ui://botworkui/state/abc123`).  This helper returns the `<extension>`
/// part so the worker can call `_goose/unstable/resources/read` even when
/// `_meta.goose.toolCall.extensionName` is missing.
///
/// Why this matters: on **live** tool calls goose ships
/// `_meta.goose.toolCall.extensionName` alongside the `ToolCall` create,
/// so the merged snapshot carries it through to `on_tool_call_completed`.
/// On **history replay** goose sends a single completed `ToolCall` for
/// each historical call and (currently) omits the `_meta` block — without
/// a fallback the resource fetch never fires and the iframe placeholder
/// stays spinning (gander#101 follow-up).
///
/// Returns `None` for URIs without a non-empty host segment.
pub(crate) fn extension_name_from_uri(uri: &str) -> Option<String> {
    let after_scheme = uri.split_once("://")?.1;
    let host = after_scheme
        .split_once('/')
        .map(|(h, _)| h)
        .unwrap_or(after_scheme);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Extension handler for goose-specific ACP extensions.
///
/// Inspects completed tool calls for `rawOutput.resourceUri` and emits an
/// [`ExtRequest::ReadResource`] so the worker can fetch the HTML panel.
///
/// The extension name comes from `_meta.goose.toolCall.extensionName` when
/// present (live tool calls); on history replay where that meta block is
/// usually absent we fall back to the host component of the URI (see
/// [`extension_name_from_uri`]).
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

        let extension_name_from_meta = tc
            .meta
            .as_ref()
            .and_then(|m| m.get("goose"))
            .and_then(|g| g.get("toolCall"))
            .and_then(|t| t.get("extensionName"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let Some(uri) = resource_uri else {
            return;
        };

        let extension_name = extension_name_from_meta
            .clone()
            .or_else(|| extension_name_from_uri(&uri));

        let Some(ext) = extension_name else {
            // No meta, and URI has no host we can use as the extension name.
            // Log so the gap is visible in wire traces rather than silently
            // dropping the fetch.
            debug!(
                target: "gander::wire",
                tool_call_id = %tc.tool_call_id,
                resource_uri = %uri,
                "READ_RESOURCE_SKIPPED_NO_EXTENSION_NAME"
            );
            return;
        };

        if extension_name_from_meta.is_none() {
            // Surface the fallback at debug level so a future regression in
            // how goose ships the meta block (or in the URI shape) shows up
            // in `RUST_LOG=gander::wire=debug` instead of silently working
            // the wrong way.
            debug!(
                target: "gander::wire",
                tool_call_id = %tc.tool_call_id,
                resource_uri = %uri,
                extension_name = %ext,
                "READ_RESOURCE_EXTENSION_NAME_FROM_URI_FALLBACK"
            );
        }

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

/// Pluck cache-hit entries out of `fetches`, emit `ToolResource` for each, and
/// return only the entries that still need an RPC.
///
/// Extracted from [`process_pending_fetches`] so the cache logic is unit
/// testable without spinning up a mock `ConnectionTo`.
async fn pluck_cached(
    fetches: Vec<(ToolCallId, String, String)>,
    cache: &ResourceCache,
    ext_ui_tx: &mpsc::Sender<ExtEvent>,
    session_id: &str,
) -> Vec<(ToolCallId, String, String)> {
    let mut remaining = Vec::with_capacity(fetches.len());
    let cache_guard = cache.lock().await;
    for (tool_call_id, uri, extension_name) in fetches {
        let key = (session_id.to_string(), uri.clone());
        if let Some(html) = cache_guard.get(&key) {
            debug!(
                target: "gander::wire",
                tool_call_id = %tool_call_id,
                uri = %uri,
                "READ_RESOURCE_CACHE_HIT"
            );
            let _ = ext_ui_tx
                .send(ExtEvent::ToolResource {
                    tool_call_id: tool_call_id.to_string(),
                    html: html.clone(),
                })
                .await;
        } else {
            remaining.push((tool_call_id, uri, extension_name));
        }
    }
    remaining
}

/// Drain `pending_fetches` and call `_goose/unstable/resources/read` for each.
///
/// Cache-hit entries (already in `cache` for the same `(session_id, uri)`) are
/// emitted as [`ExtEvent::ToolResource`] directly, with no RPC.  Cache misses
/// fire the RPC and, on success, populate the cache before emitting.
///
/// On a `text/html` response, emits [`ExtEvent::ToolResource`] via `ext_ui_tx`.
/// Errors are logged and skipped — the tool-call card remains visible without
/// an iframe.
pub async fn process_pending_fetches<R>(
    cx: &agent_client_protocol::ConnectionTo<R>,
    ext_ui_tx: &mpsc::Sender<ExtEvent>,
    pending_fetches: &PendingFetches,
    cache: &ResourceCache,
    session_id: String,
) where
    R: agent_client_protocol::role::Role,
    R: agent_client_protocol::role::HasPeer<R>,
{
    let drained: Vec<(ToolCallId, String, String)> = {
        let mut guard = pending_fetches.lock().await;
        std::mem::take(&mut *guard)
    };

    // Serve cache hits without a network round-trip.
    let fetches = pluck_cached(drained, cache, ext_ui_tx, &session_id).await;

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
            // Populate the cache before emitting so a racy second drain on the
            // same key — which can happen during fast reconnects — sees the
            // entry and short-circuits.
            cache
                .lock()
                .await
                .insert((session_id.clone(), uri.clone()), html.clone());
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

    // goose-ext: on history replay goose omits _meta.goose.toolCall.extensionName,
    // so we fall back to the host component of the resourceUri.  Without this
    // the historic panel stays at the "Loading panel…" placeholder forever.
    #[tokio::test]
    async fn completed_tool_with_resource_uri_but_no_extension_falls_back_to_uri_host() {
        let tc = ToolCall::new(ToolCallId::new("tc-no-ext"), "mcp_tool");
        let mut tc = tc;
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "ui://botworkui/state/abc"}));
        apply_tool_call_update(&mut tc, fields);

        let events = emitted_events(&tc).await;
        assert_eq!(
            events.len(),
            1,
            "missing extensionName must fall back to URI host; got {events:?}"
        );
        match &events[0] {
            ExtEvent::Request(ExtRequest::ReadResource {
                extension_name,
                uri,
                ..
            }) => {
                assert_eq!(
                    extension_name, "botworkui",
                    "extensionName must come from URI host"
                );
                assert_eq!(uri, "ui://botworkui/state/abc");
            }
            other => panic!("expected ReadResource, got {other:?}"),
        }
    }

    /// A resourceUri with no usable host (e.g. opaque URN, missing host)
    /// must skip the fetch — it isn't safe to invent an extension name.
    #[tokio::test]
    async fn completed_tool_with_unusable_resource_uri_does_not_emit() {
        let tc = ToolCall::new(ToolCallId::new("tc-bad-uri"), "mcp_tool");
        let mut tc = tc;
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(serde_json::json!({"resourceUri": "ui:///no-host/path"}));
        apply_tool_call_update(&mut tc, fields);

        let events = emitted_events(&tc).await;
        assert!(
            events.is_empty(),
            "URI with empty host must not emit; got {events:?}"
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

    // ── extension_name_from_uri ───────────────────────────────────────────

    #[test]
    fn extension_name_from_uri_returns_host_for_standard_scheme() {
        assert_eq!(
            extension_name_from_uri("ui://botworkui/state/abc"),
            Some("botworkui".to_string())
        );
    }

    #[test]
    fn extension_name_from_uri_returns_host_when_no_path() {
        // No trailing path component — the host is the whole post-scheme part.
        assert_eq!(
            extension_name_from_uri("ui://botworkui"),
            Some("botworkui".to_string())
        );
    }

    #[test]
    fn extension_name_from_uri_returns_none_for_missing_scheme() {
        assert_eq!(extension_name_from_uri("botworkui/state"), None);
    }

    #[test]
    fn extension_name_from_uri_returns_none_for_empty_host() {
        assert_eq!(extension_name_from_uri("ui:///state/abc"), None);
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

    // ── pluck_cached ──────────────────────────────────────────────────────

    /// Drain a channel into a Vec without waiting.
    async fn drain<T>(rx: &mut mpsc::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    #[tokio::test]
    async fn pluck_cached_returns_all_when_cache_empty() {
        let cache = new_resource_cache();
        let (tx, mut rx) = mpsc::channel(8);
        let fetches = vec![
            (ToolCallId::new("tc-a"), "uri-a".into(), "ext".into()),
            (ToolCallId::new("tc-b"), "uri-b".into(), "ext".into()),
        ];

        let remaining = pluck_cached(fetches, &cache, &tx, "session-1").await;

        assert_eq!(
            remaining.len(),
            2,
            "empty cache → all entries should require RPC"
        );
        assert!(
            drain(&mut rx).await.is_empty(),
            "empty cache → no ToolResource events should be emitted"
        );
    }

    #[tokio::test]
    async fn pluck_cached_emits_and_filters_cache_hits() {
        let cache = new_resource_cache();
        cache
            .lock()
            .await
            .insert(("session-1".into(), "uri-a".into()), "<p>a</p>".into());
        let (tx, mut rx) = mpsc::channel(8);
        let fetches = vec![
            (ToolCallId::new("tc-a"), "uri-a".into(), "ext".into()),
            (ToolCallId::new("tc-b"), "uri-b".into(), "ext".into()),
        ];

        let remaining = pluck_cached(fetches, &cache, &tx, "session-1").await;

        assert_eq!(remaining.len(), 1, "uri-b should remain (cache miss)");
        assert_eq!(
            remaining[0].1, "uri-b",
            "the remaining entry should be the cache miss"
        );
        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1, "exactly one cache hit should be emitted");
        match &emitted[0] {
            ExtEvent::ToolResource { tool_call_id, html } => {
                assert_eq!(tool_call_id, "tc-a");
                assert_eq!(html, "<p>a</p>");
            }
            other => panic!("expected ToolResource for cache hit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pluck_cached_keys_on_session_id() {
        // The same uri cached under a different session id must miss.
        let cache = new_resource_cache();
        cache
            .lock()
            .await
            .insert(("other-session".into(), "uri-a".into()), "<p>a</p>".into());
        let (tx, mut rx) = mpsc::channel(8);
        let fetches = vec![(ToolCallId::new("tc-a"), "uri-a".into(), "ext".into())];

        let remaining = pluck_cached(fetches, &cache, &tx, "session-1").await;

        assert_eq!(
            remaining.len(),
            1,
            "different session must not hit a foreign-session cache entry"
        );
        assert!(
            drain(&mut rx).await.is_empty(),
            "no emit expected on session-keyed cache miss"
        );
    }
}
