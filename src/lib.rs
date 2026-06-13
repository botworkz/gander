// SPDX-License-Identifier: GPL-3.0-or-later

//! Gander library target — exposes the ACP client types for integration tests.
//!
//! The main executable (`src/main.rs`) has its own `mod acp;` declaration and
//! links against libcosmic/iced directly.  This library target exposes only
//! the ACP module so that integration tests in `tests/` can drive
//! `AcpConnection` without depending on the GUI stack.

pub mod acp;
