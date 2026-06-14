// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure-ACP UI layer: data model, event handling, and components.
//!
//! This module contains everything in the Leptos chat client that is not
//! specific to any particular agent backend.  Code that depends on
//! backend-private APIs lives in the extension module instead.

pub mod components;
pub mod events;
pub mod types;

pub use types::{ChatMessage, Role, SessionEntry};
