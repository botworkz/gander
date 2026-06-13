# Integration tests

## Overview

This directory contains end-to-end integration tests for gander.  Each test
starts a real `geesed` daemon (via the `geesed` library crate), connects the
real `AcpConnection`, and exercises the full ACP v1 path — without touching
the GUI layer.

## Test files

| File | What it tests |
|---|---|
| `session_load_drain.rs` | `session/load` history-drain completion: that `drain_history_replay` terminates and emits the expected events when the agent sends all history notifications before the `session/load` response. |

## Infrastructure choices

### Why a mock agent instead of real goose?

Real goose requires model API keys, has non-deterministic responses, and is
slow.  The test contract we care about is the *ACP-level* contract: notifications
arrive before the `session/load` response.  A mock that sends known notifications
in a known order verifies exactly that contract and fails deterministically if
anything breaks it.

Using a mock does not prevent catching goose-side regressions at the protocol
level, because the contract is specified by the ACP schema — if goose stops
sending notifications before the response, a test that exercises the gander
side against a conformant mock still tells us gander is correct.  A separate
conformance test against real goose (gated on `$GOOSE_API_KEY`) would be the
right place to catch goose-side regressions, but that is out of scope here.

### Mock agent binary (`tests/fixtures/mock_acp_agent.rs`)

Compiled as `[[bin]] name = "mock-acp-agent"` in the workspace.  Referenced
from the integration test via `env!("CARGO_BIN_EXE_mock-acp-agent")`.

The binary speaks ACP v1 over stdin/stdout:

1. `initialize` → responds with protocol version 1
2. `session/list` → returns an empty session list, forcing gander to call
   `session/new`
3. `session/new` → returns a fixed session ID (`SESSION_NEW_ID`)
4. `session/load` → emits **4 history notifications before the response**:
   - `user_message_chunk`: "What is 2 + 2?"
   - `tool_call`: "calculator" with `{"op":"add","a":2,"b":2}`
   - `tool_call_update`: tool call ID "tc-1" with `{"result":4}`
   - `agent_message_chunk`: "The answer is 4."

   The notifications carry the session ID from the load request, not
   `SESSION_NEW_ID`.  This is deliberate: it means the ACP SDK has no
   pre-registered handler for that session when they arrive, so they are
   queued with `retry = true`.  When `attach_session` is called after
   `block_task().await` resolves, the queued notifications are flushed into
   the session channel — and `drain_history_replay` reads them all with
   `Duration::ZERO`.

### Why two session IDs?

If the mock returned the same session ID for both `session/new` and
`session/load`, gander would register an ACP session handler for that ID at
startup, and the handler would consume the `session/load` notifications before
the replay drain runs.  By using a *different* ID for the history session, the
ACP SDK queues the notifications (no handler yet) and flushes them only when
`attach_session` creates a new handler.

### geesed as a library dependency

`geesed` is added to `[dev-dependencies]` so the test can call
`geesed::run(RunOpts::...)` directly in a `tokio::spawn`, without shelling out
to a geesed binary.  This keeps the test self-contained and avoids PATH or
installation dependencies.

### XDG_RUNTIME_DIR manipulation

`AcpConnection::connect` derives the geesed socket path from
`$XDG_RUNTIME_DIR/geese/acp.sock`.  The test sets `XDG_RUNTIME_DIR` to the
`tempdir` root so that gander finds the socket at the same path that geesed
creates it.

A `static ENV_LOCK: tokio::sync::Mutex<()>` serialises all tests in this file
that touch `XDG_RUNTIME_DIR`, preventing data races in the (currently
hypothetical) case of parallel test execution.

## CI

The integration test is included in the standard `cargo test -p gander` run.
It requires no API keys or external services — only the `mock-acp-agent`
binary (built automatically by Cargo) and the `geesed` crate (pulled as a
dev-dependency from the same git repository as `geese` and `geese-client`).

The test has a 10-second wall-clock budget to complete.  If `drain_history_replay`
hangs (e.g., because the mock stops sending notifications before the response),
the timeout fires and the test fails with a clear message.
