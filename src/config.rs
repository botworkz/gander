// SPDX-License-Identifier: GPL-3.0-or-later

//! Cosmic-config backed user preferences for gander.
//!
//! Goose binary resolution is now delegated to geesed via `$GEESE_GOOSE_BIN`;
//! gander no longer owns that setting.

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};

#[derive(Clone, Debug, Default, CosmicConfigEntry, Eq, PartialEq)]
#[version = 2]
pub struct Config {}
