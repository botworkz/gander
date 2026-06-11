// SPDX-License-Identifier: GPL-3.0-or-later

//! Tab content rendering.
//!
//! Each open tab is bound to a `geese` profile by name. The body is currently
//! a minimal placeholder — once embedded goose UI lands (see
//! [`DESIGN.md`](../DESIGN.md)), the body becomes a webview hosting the React
//! UI for `goosed` running with this tab's `GOOSE_PATH_ROOT`.
//!
//! Per-profile *configuration* (path, status, parent, launch) lives in the
//! app's context drawer, not in the tab body — see
//! `AppModel::view_profile_config`. The tab body deliberately does not show
//! that information so the space reads as "this is where goose goes" rather
//! than "this is a settings page".
//!
//! The view returned here is intentionally a single `Element` so swapping the
//! placeholder for an embedded webview later is a localized change.

use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;
use geese::Storage;

use crate::app::Message;
use crate::fl;

/// One open tab.
///
/// Holds only what's needed for the *body* — the profile name (so we know
/// which profile to bind to) and the most recent launch error (so we can
/// surface failures inline without a separate dialog). Profile metadata is
/// resolved against `Storage` at render time; nothing is cached here.
#[derive(Clone, Debug)]
pub struct Tab {
    pub profile: String,
    pub last_launch_error: Option<String>,
}

impl Tab {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            last_launch_error: None,
        }
    }

    /// Render the tab body. Checks that the profile still exists on disk; if
    /// it doesn't, surfaces a friendly "no longer exists" message rather than
    /// panicking.
    pub fn view<'a>(&'a self, storage: &Storage) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();

        if storage.get(&self.profile).is_err() {
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

        // Placeholder until embedded goose UI lands. Just confirms which
        // profile this tab is bound to so the user knows the tab strip is
        // doing what they expect.
        let mut column = widget::column::with_capacity(2)
            .spacing(space.space_xs)
            .align_x(Alignment::Center)
            .push(widget::text::title2(format!("goose: {}", self.profile)));

        if let Some(error) = &self.last_launch_error {
            column = column.push(widget::text::body(fl!(
                "profile-config-launch-failed",
                error = error.as_str()
            )));
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
