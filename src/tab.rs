// SPDX-License-Identifier: GPL-3.0-or-later

//! Tab content rendering.
//!
//! Each open tab is bound to a `geese` profile by name. Resolution of the
//! profile (via `geese::Storage`) happens inside [`Tab::view`] so that the
//! pane reflects the current state of disk every render — `gander` does not
//! cache profile metadata.
//!
//! The view returned here is intentionally a single `Element` so the body of
//! a tab can later be swapped to an embedded webview (see DESIGN.md) without
//! touching call sites.

use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;
use geese::Storage;

use crate::app::Message;
use crate::fl;

/// One open tab. Currently just the bound profile name plus the most recent
/// launch error (if any), so we can surface failures without a separate
/// dialog.
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

    /// Render the tab body. Reads the profile from `storage` on every call;
    /// missing profiles render a friendly "no longer exists" placeholder
    /// rather than panicking.
    pub fn view<'a>(&'a self, storage: &Storage) -> Element<'a, Message> {
        let space = cosmic::theme::spacing();

        let Ok(profile) = storage.get(&self.profile) else {
            return widget::container(
                widget::column::with_children(vec![
                    widget::text::title3(self.profile.clone()).into(),
                    widget::text::body(format!(
                        "Profile {:?} no longer exists. Close this tab and pick another.",
                        self.profile
                    ))
                    .into(),
                ])
                .spacing(space.space_s),
            )
            .padding(space.space_m)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        };

        let meta = profile.meta();
        let path_display = profile.path().display().to_string();

        let mut details = widget::settings::section().title(self.profile.clone());

        details = details.add(
            widget::settings::item::builder(fl!("profile-tab-path"))
                .control(widget::text::body(path_display)),
        );

        let status = if meta.locked {
            fl!("profile-tab-status-locked")
        } else {
            fl!("profile-tab-status-unlocked")
        };
        details = details.add(
            widget::settings::item::builder(fl!("profile-tab-status"))
                .control(widget::text::body(status)),
        );

        if let Some(parent) = &meta.parent {
            details = details.add(
                widget::settings::item::builder(fl!("profile-tab-parent"))
                    .control(widget::text::body(parent.clone())),
            );
        }

        let launch = widget::button::suggested(fl!("profile-tab-launch"))
            .on_press(Message::LaunchGoose(self.profile.clone()));

        let actions = widget::row::with_capacity(2)
            .spacing(space.space_xs)
            .align_y(Alignment::Center)
            .push(launch)
            .push(widget::space::horizontal());

        let mut column = widget::column::with_capacity(3)
            .spacing(space.space_m)
            .push(details)
            .push(actions);

        if let Some(error) = &self.last_launch_error {
            column = column.push(widget::text::body(fl!(
                "profile-tab-launch-failed",
                error = error.as_str()
            )));
        }

        widget::container(column)
            .padding(space.space_m)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
