// SPDX-License-Identifier: GPL-3.0-or-later

//! Integration test: `session/load` history-drain completion.
//!
//! Verifies that `AcpConnection` correctly drains all history notifications
//! emitted by goose during a `session/load` reply, and that the sequence of
//! [`AcpEvent`]s produced matches the seeded history.
//!
//! This is the regression guard for the fix in PR #48: if goose (or the mock)
//! stops sending notifications *before* the `session/load` response, the
//! buffered-channel mechanism breaks and `drain_history_replay` would hang
//! (or miss events).
//!
//! # Infrastructure
//!
//! - [`geesed::run`] — geesed daemon running in a tokio task; no real goose
//!   binary required.
//! - `mock-acp-agent` — a minimal ACP v1 server compiled from
//!   `tests/fixtures/mock_acp_agent.rs`.  It handles `initialize`,
//!   `session/list` (returns empty), `session/new`, and `session/load` (emits
//!   4 history notifications *before* responding so the ACP SDK queues them
//!   ahead of the response).
//! - [`AcpConnection`] — the real gander ACP client, exercising the full
//!   `drain_history_replay` path.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gander::acp::{AcpCommand, AcpConnection, AcpEvent};
use gander::ext::goose::GooseExtHandler;
use gander::transport::geesed::GeesedTransport;
use geesed::{RunOpts, run};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

/// Guards concurrent mutation of `XDG_RUNTIME_DIR` across tokio tests.
/// Each test must hold this lock for the duration of any `set_var` /
/// `remove_var` calls so that parallel tests don't trample each other.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// The initial session ID is defined in `tests/fixtures/mock_acp_agent.rs`
// as `SESSION_NEW_ID` ("…0001").  The test uses a *different* ID below so
// that there is no pre-registered ACP session handler when `session/load`
// is sent: the SDK then queues the notifications (retry = true) and flushes
// them when `attach_session` registers a new handler for the history session.

/// Session ID that the test loads via `SessionSelect`.
/// Must differ from the mock's `SESSION_NEW_ID` ("…0001") — see above.
const HISTORY_SESSION_ID: &str = "00000000-0000-0000-0000-000000000002";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn runtime_dir(root: &Path) -> PathBuf {
    // Place the geesed sockets in a `geese/` subdirectory so that setting
    // `XDG_RUNTIME_DIR = root` makes gander resolve its socket path as
    // `$XDG_RUNTIME_DIR/geese/acp.sock`, which matches geesed's layout.
    root.join("geese")
}

fn acp_socket_path(root: &Path) -> PathBuf {
    runtime_dir(root).join("acp.sock")
}

fn control_socket_path(root: &Path) -> PathBuf {
    runtime_dir(root).join("control.sock")
}

async fn wait_for_socket(path: &Path) {
    timeout(Duration::from_secs(10), async {
        loop {
            if path.exists() {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for socket to appear");
}

async fn spawn_daemon(
    root: &Path,
    goose_bin: PathBuf,
) -> (
    watch::Sender<bool>,
    JoinHandle<Result<(), geesed::RunError>>,
) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(run(RunOpts::default()
        .with_runtime_dir(runtime_dir(root))
        .with_geese_root(root)
        .with_goose_bin(goose_bin)
        .with_shutdown(shutdown_rx)));
    wait_for_socket(&control_socket_path(root)).await;
    wait_for_socket(&acp_socket_path(root)).await;
    (shutdown_tx, task)
}

async fn read_json_line(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("read control socket line");
    serde_json::from_str(line.trim()).expect("parse JSON from control socket")
}

async fn send_json_line(writer: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) {
    let mut s = value.to_string();
    s.push('\n');
    writer
        .write_all(s.as_bytes())
        .await
        .expect("write to control socket");
}

/// Create a geese profile via the geesed control socket.
async fn create_profile(root: &Path, name: &str) {
    let stream = UnixStream::connect(control_socket_path(root))
        .await
        .expect("connect to control socket");
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    send_json_line(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "profile.create",
            "params": {"name": name}
        }),
    )
    .await;

    let resp = read_json_line(&mut reader).await;
    assert!(
        resp.get("result").is_some(),
        "profile.create failed: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Verify that `AcpConnection` drains all history events from a `session/load`
/// response and that the correct [`AcpEvent`] sequence is emitted.
///
/// Failure mode caught: if the mock (or a future real goose) stops emitting
/// notifications *before* the `session/load` response, `drain_history_replay`
/// would time out or miss events instead of completing promptly.
#[tokio::test]
async fn history_drain_completes_with_expected_events() {
    let root = tempdir().expect("create temp dir");
    let goose_bin = PathBuf::from(env!("CARGO_BIN_EXE_mock-acp-agent"));

    let (_shutdown_tx, _task) = spawn_daemon(root.path(), goose_bin).await;
    create_profile(root.path(), "test").await;

    // Hold the env lock for the rest of the test so XDG_RUNTIME_DIR is stable.
    let _env_guard = ENV_LOCK.lock().await;
    let original_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    // SAFETY: ENV_LOCK is held, so no other thread is reading or writing
    // XDG_RUNTIME_DIR concurrently.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", root.path()) };

    let mut conn = AcpConnection::connect(
        Box::new(GeesedTransport::new("test")),
        Box::new(GooseExtHandler),
    )
    .await
    .expect("AcpConnection::connect");

    // Send SessionSelect with a session ID that has no existing ACP handler.
    // The mock emits notifications for this ID before sending the response, so
    // they are queued in the SDK before block_task().await resolves — then
    // drain_history_replay reads them all with Duration::ZERO.
    conn.send
        .send(AcpCommand::SessionSelect(HISTORY_SESSION_ID.to_string()))
        .await
        .expect("send SessionSelect");

    // Collect all events until SessionLoadEnd, with a generous wall-clock
    // budget.  A hang here means drain_history_replay did not terminate.
    let events = timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let ev = conn
                .recv
                .recv()
                .await
                .expect("AcpEvent channel closed unexpectedly");
            let done = matches!(ev, AcpEvent::SessionLoadEnd);
            events.push(ev);
            if done {
                return events;
            }
        }
    })
    .await
    .expect("timed out before SessionLoadEnd — drain_history_replay did not complete");

    // Restore XDG_RUNTIME_DIR.
    // SAFETY: ENV_LOCK is still held.
    match original_xdg {
        Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
        None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
    }

    // --- Assertions ---------------------------------------------------------

    // Outer envelope: load start and end must be present.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::SessionLoadStart)),
        "expected SessionLoadStart in events; got: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, AcpEvent::SessionLoadEnd)),
        "expected SessionLoadEnd in events; got: {events:?}"
    );

    // History content emitted by the mock in order.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::UserText(t) if t == "What is 2 + 2?")),
        "expected UserText('What is 2 + 2?') in events; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::ToolCall(tc) if tc.title == "calculator")),
        "expected ToolCall(calculator) in events; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::ToolCall(tc) if tc.tool_call_id.0.as_ref() == "tc-1" && tc.raw_output.is_some())),
        "expected ToolCall(tc-1) with raw_output in events; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::AgentText(t) if t == "The answer is 4.")),
        "expected AgentText('The answer is 4.') in events; got: {events:?}"
    );

    // No spurious session ID: the initial session (SESSION_NEW_ID) must not
    // appear in events — it has its own handler and should never see "002"'s
    // notifications.
    assert!(
        !events.iter().any(|e| matches!(e, AcpEvent::Error(_))),
        "unexpected Error event in events; got: {events:?}"
    );

    // Verify that start comes before any history events and end comes last.
    let start_pos = events
        .iter()
        .position(|e| matches!(e, AcpEvent::SessionLoadStart))
        .unwrap();
    let end_pos = events
        .iter()
        .position(|e| matches!(e, AcpEvent::SessionLoadEnd))
        .unwrap();
    assert!(
        start_pos < end_pos,
        "SessionLoadStart must precede SessionLoadEnd; positions: {start_pos}, {end_pos}"
    );

    // SessionActive(history_id) must arrive *before* SessionLoadStart so the
    // sidebar's active highlight can flip immediately on click rather than
    // waiting for history replay to finish.  Regression guard: without this
    // ordering the user has no visual confirmation of which session they
    // selected until the entire load completes.
    let active_pos = events
        .iter()
        .position(|e| matches!(e, AcpEvent::SessionActive(id) if id == HISTORY_SESSION_ID));
    assert!(
        active_pos.is_some(),
        "expected SessionActive({HISTORY_SESSION_ID}) in events; got: {events:?}"
    );
    assert!(
        active_pos.unwrap() < start_pos,
        "SessionActive must precede SessionLoadStart; positions: {}, {start_pos}",
        active_pos.unwrap()
    );

    // All history events must fall between start and end.
    let history_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e,
                AcpEvent::UserText(_) | AcpEvent::AgentText(_) | AcpEvent::ToolCall(_)
            )
        })
        .map(|(i, _)| i)
        .collect();
    assert!(!history_indices.is_empty(), "no history events found");
    for idx in &history_indices {
        assert!(
            *idx > start_pos && *idx < end_pos,
            "history event at index {idx} is outside [{start_pos}, {end_pos}]"
        );
    }
}
