// SPDX-License-Identifier: GPL-3.0-or-later

//! Tab content rendering.
//!
//! Each open tab is bound to a `geese` profile by name. The body is a webview
//! hosted by the single process-wide [`iced_webview::Cef`] engine owned by
//! `AppModel`. Each tab holds only an opaque [`iced_webview::ViewId`] for its
//! view inside that engine.
//!
//! Per-profile *configuration* (path, status, parent, launch) lives in the
//! app's context drawer, not in the tab body — see
//! `AppModel::view_profile_config`.

use cosmic::iced::{Alignment, Length};
use cosmic::prelude::*;
use cosmic::widget;
use geese::Storage;
use iced_webview::ViewId;

use crate::app::Message;
use crate::fl;

/// One open tab.
///
/// Holds only what's needed for the *body* — the profile name (so we know
/// which profile to bind to) and the most recent launch error (so we can
/// surface failures inline without a separate dialog). The actual webview
/// view is owned by the single `WebView<Cef, _>` in `AppModel`; this struct
/// stores only the opaque `ViewId` assigned to this tab.
pub struct Tab {
    pub profile: String,
    pub last_launch_error: Option<String>,
    /// The ViewId inside `AppModel::webview` for this tab's browser view.
    /// `None` until the async `CreateView` task completes.
    pub view_id: Option<ViewId>,
}

impl Tab {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            last_launch_error: None,
            view_id: None,
        }
    }

    /// Render the tab body.
    ///
    /// `webview_body` is the element produced by `AppModel::webview.view(id)`
    /// for this tab's `view_id`, or `None` if the view is not yet live.
    pub fn view<'a>(
        &'a self,
        storage: &Storage,
        webview_body: Option<Element<'a, Message>>,
    ) -> Element<'a, Message> {
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

        let body: Element<'_, Message> = webview_body.unwrap_or_else(|| {
            widget::container(widget::text::body(format!("goose: {}", self.profile)))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        });

        let mut column = widget::column::with_capacity(2);
        if let Some(error) = &self.last_launch_error {
            column = column.spacing(space.space_xs).push(widget::text::body(fl!(
                "profile-config-launch-failed",
                error = error.as_str()
            )));
        }

        widget::container(column.push(body))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// Build a `data:` URL that renders a minimal HTML page for `profile`.
pub fn data_url_for_profile(profile: &str) -> String {
    let html = format!("<h1>goose: {}</h1>", html_escape(profile));
    format!(
        "data:text/html;charset=utf-8,{}",
        percent_encode(html.as_bytes())
    )
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~' => encoded.push(char::from(*byte)),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{data_url_for_profile, html_escape};

    #[test]
    fn escapes_profile_name_for_html() {
        assert_eq!(
            html_escape("<work>&\"'"),
            "&lt;work&gt;&amp;&quot;&#39;"
        );
    }

    #[test]
    fn builds_data_url_from_escaped_html() {
        let url = data_url_for_profile("<tab>");
        assert_eq!(
            url,
            "data:text/html;charset=utf-8,%3Ch1%3Egoose%3A%20%26lt%3Btab%26gt%3B%3C%2Fh1%3E"
        );
    }
}

