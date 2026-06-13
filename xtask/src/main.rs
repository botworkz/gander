// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    path::Path,
    process::{Command, ExitCode},
};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "gander build tasks")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build the gander-chat Leptos WASM bundle via trunk.
    BuildChat {
        /// Build in release mode.
        #[arg(long)]
        release: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::BuildChat { release } => build_chat(release),
    }
}

fn build_chat(release: bool) -> ExitCode {
    // Probe for trunk on PATH before trying to run the real build.
    let trunk_ok = Command::new("trunk")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !trunk_ok {
        eprintln!(
            "error: `trunk` not found on PATH.\n\
             Install it with:  cargo install trunk\n\
             Then re-run:      cargo xtask build-chat"
        );
        return ExitCode::FAILURE;
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root");

    let chat_dir = workspace_root.join("crates").join("gander-chat");

    let mut cmd = Command::new("trunk");
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(&chat_dir);

    let status = cmd.status().expect("failed to spawn trunk");
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
