// SPDX-License-Identifier: GPL-3.0-or-later

//! Goose-specific UI layer: extension events and components.
//!
//! This module contains everything that depends on goose-private APIs or
//! surfaces: the `tool_resource` bridge event, the MCP App iframe, and
//! the Extensions/Settings concertina.  Pure-ACP code lives in
//! `crate::acp_core` instead.

pub mod components;
pub mod events;

pub use components::{Concertina, McpAppIframe};
