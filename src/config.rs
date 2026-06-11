// SPDX-License-Identifier: GPL-3.0-or-later

//! Cosmic-config backed user preferences for gander.
//!
//! Currently minimal — the only stored preference is which `goose` binary to
//! invoke when launching a profile. Defaults to the literal string `"goose"`
//! (resolved against `$PATH`).

use cosmic::cosmic_config::{self, cosmic_config_derive::CosmicConfigEntry, CosmicConfigEntry};

#[derive(Clone, Debug, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    /// Path or name of the `goose` binary to spawn for a profile. Defaults to
    /// `"goose"` so the system `$PATH` resolves it.
    pub goose_bin: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            goose_bin: "goose".to_owned(),
        }
    }
}
