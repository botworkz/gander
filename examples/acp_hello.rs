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

use std::str::FromStr;

use agent_client_protocol::{AcpAgent, Client};
use agent_client_protocol::schema::{InitializeRequest, ProtocolVersion};

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

    eprintln!("Spawning agent: {binary}");

    let agent = AcpAgent::from_str(&binary)
        .map_err(|e| format!("Failed to configure agent '{binary}': {e}"))?;

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
                    print!("{response}");

                    Ok(())
                })
                .await
        })
        .await
        .map_err(|e| format!("ACP session failed: {e}"))?;

    Ok(())
}
