// SPDX-License-Identifier: GPL-3.0-or-later

//! Mock ACP v1 agent for integration testing.
//!
//! Invoked by geesed as the "goose" binary: `mock-acp-agent acp`.
//! Communicates over stdin/stdout using newline-delimited JSON-RPC 2.0.
//!
//! Session protocol handled:
//! - `initialize`    → protocol-version 1 response
//! - `session/list`  → empty list (forces the `session/new` path in gander)
//! - `session/new`   → fixed session ID `SESSION_NEW_ID`
//! - `session/load`  → emits 4 history notifications *before* the response.
//!   The ACP SDK queues them before `block_task().await` resolves; this is
//!   the contract that `drain_history_replay` relies on.
//! - anything else   → JSON-RPC method-not-found error

use std::io::{BufRead, BufReader, Write};

/// Session ID returned for `session/new`.  Different from `HISTORY_SESSION_ID`
/// in the integration test so that there is no pre-existing session handler
/// when the test triggers `session/load` on the history session.
const SESSION_NEW_ID: &str = "00000000-0000-0000-0000-000000000001";

fn write_json_line(writer: &mut impl Write, value: &serde_json::Value) {
    let s = serde_json::to_string(value).expect("serialize JSON");
    writer.write_all(s.as_bytes()).expect("write JSON");
    writer.write_all(b"\n").expect("write newline");
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or("");

        if method == "session/load" {
            // The session ID the client wants to load.
            let session_id = req["params"]["sessionId"]
                .as_str()
                .unwrap_or("")
                .to_string();

            // Emit history notifications *before* the response.  This
            // guarantees the ACP SDK has queued all of them before
            // block_task().await resolves, so drain_history_replay sees a
            // pre-filled channel and can drain with Duration::ZERO.
            let notifications = [
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "user_message_chunk",
                            "content": {"type": "text", "text": "What is 2 + 2?"}
                        }
                    }
                }),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": "tc-1",
                            "title": "calculator",
                            "rawInput": {"op": "add", "a": 2, "b": 2}
                        }
                    }
                }),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": "tc-1",
                            "rawOutput": {"result": 4}
                        }
                    }
                }),
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "The answer is 4."}
                        }
                    }
                }),
            ];

            for notif in &notifications {
                write_json_line(&mut writer, notif);
            }
            // Flush all notifications before sending the response.
            writer.flush().expect("flush notifications");

            write_json_line(
                &mut writer,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            );
            writer.flush().expect("flush response");
            continue;
        }

        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {},
                    "agentInfo": {"name": "mock-acp-agent", "version": "0.1.0"}
                }
            }),
            "session/list" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"sessions": [], "nextCursor": null}
            }),
            "session/new" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"sessionId": SESSION_NEW_ID}
            }),
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }),
        };

        write_json_line(&mut writer, &response);
        writer.flush().expect("flush response");
    }
}
