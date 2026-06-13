// SPDX-License-Identifier: GPL-3.0-or-later

//! Tab content rendering.
//!
//! Each open tab is bound to a `geese` profile by name. The body is a
//! placeholder while the embedded WebView loads, and shows an error state
//! when ACP connectivity fails.
//!
//! Per-profile *configuration* (path, status, parent) lives in the app's
//! context drawer, not in the tab body — see `AppModel::view_profile_config`.

use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;
use geese_client::ProfileEntry;

use crate::app::Message;
use crate::fl;

/// One open tab.
///
/// Holds only what's needed for the *body* — the profile name and the most
/// recent ACP error string.  Profile metadata is looked up against the
/// cached `known_profiles` slice at render time.
#[derive(Clone, Debug)]
pub struct Tab {
    pub profile: String,
    /// Set when the ACP connection fails so the tab body can show an error.
    pub acp_error: Option<String>,
}

impl Tab {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            acp_error: None,
        }
    }

    /// Render the tab body.
    ///
    /// Checks that the profile is still known; if it has been deleted,
    /// surfaces a friendly "no longer exists" message rather than panicking.
    pub fn view<'a>(&'a self, known_profiles: &[ProfileEntry]) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();

        let profile_exists = known_profiles.iter().any(|p| p.name == self.profile);
        if !profile_exists {
            return widget::container(
                widget::column::with_children(vec![
                    widget::text::title3(self.profile.clone()).into(),
                    widget::text::body(fl!("tab-placeholder-missing")).into(),
                ])
                .spacing(space.space_s)
                .align_x(Alignment::Center),
            )
            .padding(space.space_xl)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into();
        }

        // On Linux the WebView covers this element; it is visible only on
        // non-Linux builds or before the WebView paints for the first time.
        let mut column = widget::column::with_capacity(2)
            .spacing(space.space_xs)
            .align_x(Alignment::Center)
            .push(widget::text::title2(format!("goose: {}", self.profile)));

        if let Some(error) = &self.acp_error {
            column = column.push(widget::text::body(error.clone()));
        }

        widget::container(column)
            .padding(space.space_xl)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }
}
