// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike: prove ACP v1 over stdio from Rust.
//!
//! Spawns a goose agent process, opens an ACP v1 session over the child's stdio,
//! sends a single prompt, prints the streamed response, and exits cleanly.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example acp_hello -- /path/to/goose-binary
//! ```
//!
//! The argument is just the goose binary path; the example appends the `acp`
//! subcommand itself. All JSON-RPC traffic and the child's stderr are mirrored
//! to our stderr via [`AcpAgent::with_debug`] so failures pre-handshake are
//! visible instead of silent.

use std::io::Write;

use agent_client_protocol::schema::{InitializeRequest, ProtocolVersion};
use agent_client_protocol::{AcpAgent, Client};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let binary = std::env::args()
        .nth(1)
        .ok_or("Usage: acp_hello <path-to-goose-binary>")?;

    eprintln!("spawning agent: {binary} acp");

    // goose only speaks ACP on stdio when invoked as `goose acp`; without the
    // subcommand it tries to launch the TUI on the piped tty and the handshake
    // never starts. AcpAgent::from_args takes each element as a literal arg
    // (no shell splitting) so a path containing spaces is still safe.
    let agent = AcpAgent::from_args([binary.as_str(), "acp"])
        .map_err(|e| format!("failed to configure agent '{binary} acp': {e}"))?
        .with_debug(|line, direction| {
            // Mirror both directions of the JSON-RPC stream and any agent
            // stderr to our stderr. Without this, AcpAgent buffers stderr
            // internally and only surfaces it on a non-zero child exit, so a
            // hang during the handshake produces no output at all.
            eprintln!("[{direction:?}] {line}");
        });

    Client
        .builder()
        .name("acp-hello")
        .connect_with(agent, async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            cx.build_session_cwd()?
                .block_task()
                .run_until(async |mut session| {
                    session.send_prompt("hello, are you there?")?;

                    let response = session.read_to_string().await?;
                    println!("{response}");
                    // println! is line-buffered when stdout is a tty but fully
                    // buffered when redirected; flush explicitly so the
                    // response is visible even when piped.
                    let _ = std::io::stdout().flush();

                    Ok(())
                })
                .await
        })
        .await
        .map_err(|e| format!("ACP session failed: {e}"))?;

    Ok(())
}
